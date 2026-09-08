#!/usr/bin/env python3
"""Supplemental production-profile protocol; leaves the reviewed baseline harness unchanged."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import sqlite3
import tempfile

import duckdb

import catalog_benchmark as bench

PROTOCOL_VERSION = 1
PROFILES_MIB = (256, 1024, 2048)
ACCEPTANCE_SCALES = (1_000_000, 5_000_000, 10_000_000)
PAGE_NAMES = set(bench.QUERY_SQL) - {"aggregate"}
RSS_LIMIT = 4 * 1024**3


def digest(path):
    with Path(path).open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def readonly(engine, path):
    if engine == "sqlite":
        return sqlite3.connect(
            Path(path).resolve().as_uri() + "?mode=ro", uri=True, isolation_level=None
        )
    return duckdb.connect(
        str(path), read_only=True, config={"memory_limit": "4096MiB", "threads": 4}
    )


def verify_pristine(db, expected):
    proof = bench.verify(db, expected)
    require(
        db.execute("SELECT count(*) FROM edits").fetchone()[0] == 0,
        "edited database is not a pristine pre-mixed snapshot",
    )
    require(
        db.execute("SELECT value FROM recovery_probe WHERE id=1").fetchone()[0] == 0,
        "database recovery probe is no longer pristine",
    )
    require(
        db.execute("SELECT count(*) FROM annotations").fetchone()[0]
        == expected["count"],
        "annotation state is missing",
    )
    return proof


def copy_checkpointed(engine, source, destination, expected):
    """Snapshot through engine locks, then verify the closed isolated copy independently."""
    source, destination = Path(source), Path(destination)
    require(source.is_file(), f"missing source database: {source}")
    require(not destination.exists(), f"copy destination already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    connection = readonly(engine, source)
    try:
        if engine == "sqlite":
            connection.execute("BEGIN")
        verify_pristine(connection, expected)
        if engine == "sqlite":
            target = sqlite3.connect(destination)
            try:
                connection.backup(target)
            finally:
                target.close()
        else:
            wal = Path(str(source) + ".wal")
            require(
                not wal.exists() or wal.stat().st_size == 0,
                "DuckDB snapshot requires a checkpointed source without a live WAL",
            )
            # A read-only DuckDB connection excludes an external writer during the copy.
            before = source.stat()
            shutil.copyfile(source, destination)
            after = source.stat()
            require(
                (before.st_size, before.st_mtime_ns)
                == (after.st_size, after.st_mtime_ns),
                "source changed while copying",
            )
    finally:
        connection.close()
    target = bench.connect(engine, destination, 4096)
    try:
        proof = verify_pristine(target, expected)
        bench.checkpoint(target, engine)
    finally:
        target.close()
    return {
        "sha256": digest(destination),
        "bytes": destination.stat().st_size,
        "proof": proof,
        "validation_memory_mib": 4096,
    }


def snapshot(source_root, output):
    source_root, output = Path(source_root).resolve(), Path(output).resolve()
    require(not output.exists(), "snapshot output must not exist")
    require(
        not output.is_relative_to(source_root)
        and not source_root.is_relative_to(output),
        "snapshot and source roots must be separate",
    )
    original_path = source_root / "campaign.json"
    original = json.loads(original_path.read_text())
    require(
        original.get("prepared_only") is True and original.get("complete") is False,
        "snapshot must precede original campaign read/mixed/recovery execution",
    )
    module_sha = digest(bench.__file__)
    require(
        original.get("script_sha256") == module_sha,
        "prepared source uses a different frozen harness",
    )
    output.parent.mkdir(parents=True, exist_ok=True)
    record = dict(
        protocol_version=PROTOCOL_VERSION,
        profiles_mib=list(PROFILES_MIB),
        counts=original["counts"],
        production_default_mib=256,
        stress_only_mib=64,
        repetitions=original["repetitions"],
        fresh_repetitions=original["fresh_repetitions"],
        driver_sha256=digest(__file__),
        frozen_harness_sha256=module_sha,
        source_manifest_sha256=digest(original_path),
        source_root=str(source_root),
        snapshot_host=bench.host(),
        datasets={},
        snapshots={},
        complete=False,
    )
    with tempfile.TemporaryDirectory(
        prefix=".profile-snapshot-", dir=output.parent
    ) as staging:
        staging = Path(staging)
        for count in original["counts"]:
            expected_path = source_root / str(count) / "csv" / "manifest.json"
            expected = json.loads(expected_path.read_text())
            require(
                expected["count"] == count
                and expected["seed"] == bench.SEED
                and expected["version"] == bench.VERSION,
                "dataset contract mismatch",
            )
            record["datasets"][str(count)] = expected
            for engine in ["sqlite", "duckdb"]:
                receipt = json.loads(
                    (source_root / str(count) / f"{engine}.json").read_text()
                )
                require(
                    set(receipt) == {"load"} and "error" not in receipt["load"],
                    "source is missing a successful preparation-only receipt",
                )
                require(
                    receipt["load"]["proof"] == expected["expected"],
                    "source preparation proof mismatch",
                )
                filename = "catalog.sqlite3" if engine == "sqlite" else "catalog.duckdb"
                relative = Path(str(count)) / engine / filename
                record["snapshots"][str(relative)] = copy_checkpointed(
                    engine, source_root / relative, staging / relative, expected
                )
        record["complete"] = True
        bench.dump(staging / "snapshot.json", record)
        require(not output.exists(), "snapshot output appeared during preparation")
        staging.rename(output)
    return record


def profile_checks(result, repetitions, fresh_repetitions):
    """One production profile; constrained64MiB results are deliberately not an eligibility gate."""
    checks = {}
    plans = result.get("plans", {})
    checks["plans"] = set(plans) == set(bench.QUERY_SQL) and all(
        isinstance(v, list) and v for v in plans.values()
    )
    warm = result.get("warm", {})
    workloads = warm.get("workloads", {})
    checks["warm_pages"] = set(workloads) == set(bench.QUERY_SQL) and all(
        workloads[name].get("distribution", {}).get("n") == repetitions
        and workloads[name]["distribution"].get("p95_ms", float("inf")) <= 100
        for name in PAGE_NAMES
    )
    checks["warm_rss"] = warm.get("peak_rss_bytes", float("inf")) <= RSS_LIMIT
    fresh = result.get("fresh", {})
    checks["fresh_pages"] = set(fresh) == PAGE_NAMES and all(
        not value.get("errors", ["missing errors field"])
        and value.get("open_plus_query", {}).get("n") == fresh_repetitions
        and value["open_plus_query"].get("p95_ms", float("inf")) <= 500
        for value in fresh.values()
    )
    checks["fresh_rss"] = set(fresh) == PAGE_NAMES and all(
        value.get("peak_rss_bytes", float("inf")) <= RSS_LIMIT
        for value in fresh.values()
    )
    mixed = result.get("mixed", {})
    checks["durable_writes"] = (
        not mixed.get("error")
        and mixed.get("errors") == []
        and mixed.get("background", {}).get("errors") == []
        and all(
            mixed.get("workloads", {}).get(name, {}).get("n") == repetitions
            and mixed["workloads"][name].get("p95_ms", float("inf")) <= 100
            for name in ["rating", "edit"]
        )
    )
    checks["recovery"] = all(
        result.get("recovery", {}).get(stage, {}).get("actual") == expected
        for stage, expected in [("before", 1), ("after", 2)]
    )
    checks["pristine_copy"] = bool(result.get("copy", {}).get("proof")) and bool(
        result.get("copy", {}).get("sha256")
    )
    return dict(checks=checks, all_pass=all(checks.values()))


def measure_case(engine, path, count, memory_mib, repetitions, fresh_repetitions):
    common = [
        "--engine",
        engine,
        "--db",
        path,
        "--count",
        count,
        "--memory-mb",
        memory_mib,
    ]
    result = {
        "plans": bench.child(["plans", *common]),
        "warm": bench.child(["read", *common, "--repetitions", repetitions]),
        "fresh": {},
    }
    for name in sorted(PAGE_NAMES):
        raw = [
            bench.child(
                [
                    "read",
                    *common,
                    "--single",
                    name,
                    "--iteration",
                    i,
                    "--repetitions",
                    1,
                ]
            )
            for i in range(fresh_repetitions)
        ]
        successes = [r for r in raw if "error" not in r]
        receipt = {"errors": [r for r in raw if "error" in r], "raw": raw}
        if successes:
            receipt.update(
                open_plus_query=bench.distribution(
                    [
                        r["open_ms"]
                        + r["workloads"][name]["distribution"]["samples_ms"][0]
                        for r in successes
                    ]
                ),
                query_only=bench.distribution(
                    [
                        r["workloads"][name]["distribution"]["samples_ms"][0]
                        for r in successes
                    ]
                ),
                peak_rss_bytes=max(r["peak_rss_bytes"] for r in successes),
            )
        result["fresh"][name] = receipt
    result["mixed"] = bench.child(["mixed", *common, "--repetitions", repetitions * 2])
    result["recovery"] = bench.child(["recovery", *common])
    return result


def run(snapshot_root, output):
    snapshot_root, output = Path(snapshot_root).resolve(), Path(output).resolve()
    require(
        not output.exists(),
        "measurement output must not exist; keep every attempted profile",
    )
    snapshot_manifest = json.loads((snapshot_root / "snapshot.json").read_text())
    require(snapshot_manifest.get("complete") is True, "incomplete pristine snapshot")
    require(
        snapshot_manifest["driver_sha256"] == digest(__file__)
        and snapshot_manifest["frozen_harness_sha256"] == digest(bench.__file__),
        "protocol changed after snapshot",
    )
    require(
        snapshot_manifest["profiles_mib"] == list(PROFILES_MIB),
        "profile ordering changed",
    )
    output.mkdir(parents=True)
    counts = snapshot_manifest["counts"]
    repetitions, fresh_repetitions = (
        snapshot_manifest["repetitions"],
        snapshot_manifest["fresh_repetitions"],
    )
    manifest = dict(
        protocol_version=PROTOCOL_VERSION,
        driver_sha256=digest(__file__),
        frozen_harness_sha256=digest(bench.__file__),
        snapshot_manifest_sha256=digest(snapshot_root / "snapshot.json"),
        snapshot_root=str(snapshot_root),
        profiles_mib=list(PROFILES_MIB),
        counts=counts,
        repetitions=repetitions,
        fresh_repetitions=fresh_repetitions,
        measurement_host=bench.host(),
        selected_profiles_mib={},
        attempts={},
        complete=False,
    )
    bench.dump(output / "production_profiles.json", manifest)
    for engine in ["sqlite", "duckdb"]:
        manifest["selected_profiles_mib"][engine] = None
        manifest["attempts"][engine] = {}
        for memory_mib in PROFILES_MIB:
            profile_passes = True
            per_scale = {}
            for count in counts:
                filename = "catalog.sqlite3" if engine == "sqlite" else "catalog.duckdb"
                relative = Path(str(count)) / engine / filename
                source = snapshot_root / relative
                require(
                    digest(source)
                    == snapshot_manifest["snapshots"][str(relative)]["sha256"],
                    "pristine snapshot changed",
                )
                case_root = output / engine / str(memory_mib) / str(count)
                target = case_root / filename
                proof = copy_checkpointed(
                    engine, source, target, snapshot_manifest["datasets"][str(count)]
                )
                result = measure_case(
                    engine, target, count, memory_mib, repetitions, fresh_repetitions
                )
                result.update(
                    copy=proof,
                    configured_memory_mib=memory_mib,
                    driver_sha256=manifest["driver_sha256"],
                    frozen_harness_sha256=manifest["frozen_harness_sha256"],
                )
                verdict = profile_checks(result, repetitions, fresh_repetitions)
                result["verdict"] = verdict
                bench.dump(case_root / "receipt.json", result)
                per_scale[str(count)] = {
                    "receipt": str((case_root / "receipt.json").relative_to(output)),
                    "verdict": verdict,
                }
                profile_passes = profile_passes and verdict["all_pass"]
            manifest["attempts"][engine][str(memory_mib)] = dict(
                all_scales_pass=profile_passes, scales=per_scale
            )
            if profile_passes:
                manifest["selected_profiles_mib"][engine] = memory_mib
            bench.dump(output / "production_profiles.json", manifest)
            if profile_passes:
                break
    manifest["complete"] = True
    manifest["acceptance_scales_present"] = tuple(counts) == ACCEPTANCE_SCALES
    manifest["measurement_host_after"] = bench.host()
    bench.dump(output / "production_profiles.json", manifest)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["snapshot", "run"])
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    result = (
        snapshot(args.source, args.output)
        if args.command == "snapshot"
        else run(args.source, args.output)
    )
    print(json.dumps(result))


if __name__ == "__main__":
    main()
