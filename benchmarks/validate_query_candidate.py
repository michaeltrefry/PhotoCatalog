#!/usr/bin/env python3
"""Versioned SQLite page-correction experiment; never edits the frozen SQL files."""

import argparse
from contextlib import contextmanager
import hashlib
import json
import math
from pathlib import Path
import subprocess
import sys

import catalog_benchmark as bench
import production_profiles as profiles
import query_candidates

VERSION = 1
ENGINE = "sqlite"
MEMORY_MIB = 256
SCALES = (1_000_000, 5_000_000, 10_000_000)
WARM_SAMPLES = 100
FRESH_SAMPLES = 20
RSS_LIMIT = 4 * 1024**3
SCRIPT = Path(__file__).resolve()
FROZEN_FILE = Path(bench.__file__).resolve()
FROZEN_SHA256 = "167b13d524c0f21a18426bb8b21acc6792f275de7884edca1cf995c6ff874c89"
BASE_SQL = bench.QUERY_SQL.copy()
PAGES = tuple(name for name in BASE_SQL if name != "aggregate")
require = profiles.require


def object_digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def identity():
    require(profiles.digest(FROZEN_FILE) == FROZEN_SHA256, "frozen harness changed")
    require(set(query_candidates.CANDIDATE_SQL) == {"page_deep", "rating"}, "unexpected correction scope")
    sql = {name: query_candidates.CANDIDATE_SQL.get(name, BASE_SQL[name]) for name in PAGES}
    require(all(sql[name] != BASE_SQL[name] for name in query_candidates.CANDIDATE_SQL), "candidate must retain both reviewed corrections")
    return {
        "protocol_version": VERSION,
        "driver_sha256": profiles.digest(SCRIPT),
        "frozen_harness_sha256": FROZEN_SHA256,
        "copy_helper_sha256": profiles.digest(profiles.__file__),
        "candidate_module_sha256": profiles.digest(query_candidates.__file__),
        "candidate_sql": sql,
        "candidate_sql_sha256": object_digest(sql),
        "base_sql_sha256": object_digest(BASE_SQL),
        "unchanged_aggregate_sql": BASE_SQL["aggregate"],
        "engine": ENGINE,
        "engine_version": bench.sqlite3.sqlite_version,
        "python": sys.version,
        "settings_requested": bench.settings(ENGINE, MEMORY_MIB),
    }


@contextmanager
def candidate_environment(trace=None):
    """Only child-process module globals change; on-disk modules stay immutable."""
    previous_sql, previous_file, previous_connect = bench.QUERY_SQL, bench.__file__, bench.connect
    bench.QUERY_SQL = identity()["candidate_sql"]
    # The frozen recovery helper dispatches crash workers through its __file__.
    # Point only that dispatch at this runner, whose parser accepts the same args.
    bench.__file__ = str(SCRIPT)
    if trace is not None:
        def traced_connect(*args, **kwargs):
            db = previous_connect(*args, **kwargs)
            db.set_trace_callback(trace.append)
            return db
        bench.connect = traced_connect
    try:
        yield
    finally:
        bench.QUERY_SQL, bench.__file__, bench.connect = previous_sql, previous_file, previous_connect


def request(operation, count, repetitions=1, single=None, iteration=0):
    return dict(operation=operation, count=count, memory_mib=MEMORY_MIB,
                repetitions=repetitions, single=single, iteration=iteration)


def worker(arguments):
    record = dict(identity=identity(), request=request(arguments.operation, arguments.count,
                  arguments.repetitions, arguments.single, arguments.iteration))
    trace = [] if arguments.trace_sql else None
    try:
        with candidate_environment(trace):
            if arguments.operation == "crash-worker":
                bench.crash_worker(ENGINE, arguments.db, MEMORY_MIB, arguments.stage)
                raise RuntimeError("crash worker unexpectedly returned")
            if arguments.operation == "read":
                record["result"] = bench.read_workload(ENGINE, arguments.db, arguments.count,
                    MEMORY_MIB, arguments.repetitions, arguments.single, arguments.iteration)
            elif arguments.operation == "plans":
                record["result"] = bench.plans(ENGINE, arguments.db, arguments.count, MEMORY_MIB)
            elif arguments.operation == "mixed":
                record["result"] = bench.mixed(ENGINE, arguments.db, arguments.count, MEMORY_MIB, arguments.repetitions)
            else:
                record["result"] = bench.recovery(ENGINE, arguments.db, MEMORY_MIB)
            if arguments.operation == "read":
                record["settings_actual"] = record["result"]["settings"]
                record["parameter_schedule"] = {
                    name: [{"iteration": item["iteration"], "parameters": bench.query_parameters(name, arguments.count, item["iteration"])}
                           for item in value["correctness"]]
                    for name, value in record["result"]["workloads"].items()
                }
            else:
                # Settings readback follows measurement; it must not prewarm fresh reads.
                db = bench.connect(ENGINE, arguments.db, MEMORY_MIB)
                try:
                    record["settings_actual"] = bench.measured_settings(db, ENGINE)
                finally:
                    db.close()
    except Exception as error:
        record["error"] = f"{type(error).__name__}: {error}"
    if trace is not None:
        record["executed_sql_trace"] = trace
    return record


def child(db, req, destination, trace_sql=False):
    command = [sys.executable, str(SCRIPT), req["operation"], "--db", str(db),
        "--count", str(req["count"]), "--repetitions", str(req["repetitions"]),
        "--iteration", str(req["iteration"])]
    if req["single"] is not None:
        command += ["--single", req["single"]]
    if trace_sql:
        command += ["--trace-sql"]
    with Path(destination).open("x") as stream:
        # communicate()-based capture drains pipes while waiting; no polling/pipe deadlock.
        completed = subprocess.run(command, text=True, capture_output=True)
        try:
            result = json.loads(completed.stdout)
            require(isinstance(result, dict), "child did not return an object")
        except (ValueError, TypeError):
            result = {"error": "child did not publish valid JSON", "stdout": completed.stdout}
        result["returncode"] = completed.returncode
        if completed.stderr:
            result["stderr"] = completed.stderr
        if completed.returncode and "error" not in result:
            result["error"] = "child exited unsuccessfully"
        json.dump(result, stream, indent=2, sort_keys=True)
        stream.write("\n")
        return result

def valid_envelope(value, expected_identity, expected_request):
    return (isinstance(value, dict) and value.get("returncode") == 0
        and "error" not in value and value.get("identity") == expected_identity
        and value.get("request") == expected_request
        and value.get("settings_actual") == expected_identity["settings_requested"])


def valid_distribution(value, samples, p95_limit=None):
    if not isinstance(value, dict) or value.get("n") != samples:
        return False
    raw = value.get("samples_ms")
    if not isinstance(raw, list) or len(raw) != samples or not raw:
        return False
    if not all(type(v) in (int, float) and math.isfinite(v) and v >= 0 for v in raw):
        return False
    computed = bench.distribution(raw)
    if not all(value.get(key) == computed[key] for key in ("n", "p50_ms", "p95_ms", "p99_ms", "max_ms")):
        return False
    return p95_limit is None or computed["p95_ms"] <= p95_limit


def page_checks(value, base, expected_request, expected_identity):
    envelope = valid_envelope(value, expected_identity, expected_request)
    data = value.get("result", {})
    workloads = data.get("workloads", {})
    names = (expected_request["single"],) if expected_request["single"] else PAGES
    correct = envelope and set(workloads) == set(names)
    samples = expected_request["repetitions"]
    schedules = {}
    for name in names:
        measured = workloads.get(name, {})
        original = base.get("workloads", {}).get(name, {})
        expected = original.get("correctness", [])
        schedules[name] = [{"iteration": item["iteration"], "parameters": bench.query_parameters(name, expected_request["count"], item["iteration"])} for item in expected]
        actual = measured.get("correctness", [])
        correct = correct and len(actual) == samples and actual == expected
        correct = correct and all(v.get("rows") == 200 for v in actual)
    correct = correct and value.get("parameter_schedule") == schedules
    distributions = correct and all(valid_distribution(workloads[name].get("distribution"), samples,
        None if expected_request["single"] else 100) for name in names)
    rss = data.get("peak_rss_bytes")
    rss_ok = type(rss) is int and 0 < rss <= RSS_LIMIT
    return {"identity_and_settings": envelope, "baseline_hashes": correct,
            "distributions": distributions, "browse_rss": rss_ok}


def evaluate(result, baseline, expected_identity, count, repetitions=WARM_SAMPLES, fresh_repetitions=FRESH_SAMPLES):
    checks = {}
    fresh_distributions = {}
    plan = result.get("plans", {})
    checks["plans"] = valid_envelope(plan, expected_identity, request("plans", count)) and set(plan.get("result", {})) == set(PAGES) and all(isinstance(v, list) and v for v in plan.get("result", {}).values())
    warm = page_checks(result.get("warm", {}), baseline["warm_256mb"], request("read", count, repetitions), expected_identity)
    checks.update({"warm_" + key: value for key, value in warm.items()})
    fresh = result.get("fresh", {})
    checks["fresh_complete"] = set(fresh) == set(PAGES)
    checks["fresh_correctness"] = checks["fresh_complete"]
    checks["fresh_latency"] = checks["fresh_complete"]
    for name in PAGES:
        values = fresh.get(name, [])
        if len(values) != fresh_repetitions:
            checks["fresh_correctness"] = checks["fresh_latency"] = False
            continue
        totals, query_samples = [], []
        for index, value in enumerate(values):
            req = request("read", count, 1, name, index)
            raw_base = baseline["fresh_process"][name]["raw"]
            base = raw_base[index] if index < len(raw_base) else {}
            page = page_checks(value, base, req, expected_identity)
            checks["fresh_correctness"] &= all(page.values())
            if all(page.values()):
                data = value["result"]
                opened = data.get("open_ms")
                if type(opened) in (int, float) and math.isfinite(opened) and opened >= 0:
                    query_ms = data["workloads"][name]["distribution"]["samples_ms"][0]
                    totals.append(opened + query_ms)
                    query_samples.append(query_ms)
        fresh_distributions[name] = {"open_plus_query": bench.distribution(totals) if totals else None,
                                     "query_only": bench.distribution(query_samples) if query_samples else None,
                                     "valid_samples": len(totals), "expected_samples": fresh_repetitions}
        checks["fresh_latency"] &= len(totals) == fresh_repetitions and bench.distribution(totals)["p95_ms"] <= 500
    mix_envelope = result.get("mixed", {})
    mixed = mix_envelope.get("result", {})
    background = mixed.get("background", {})
    checks["mixed_identity"] = valid_envelope(mix_envelope, expected_identity, request("mixed", count, repetitions * 2))
    checks["durable_writes"] = (checks["mixed_identity"] and mixed.get("errors") == []
        and background.get("errors") == [] and mixed.get("concurrent_starts") == repetitions * 2
        and type(background.get("batches")) is int and background["batches"] > 0
        and background.get("rows") == background["batches"] * 32
        and len(background.get("samples_ms", [])) == background["batches"]
        and all(valid_distribution(mixed.get("workloads", {}).get(name), repetitions, 100) for name in ("rating", "edit")))
    checks["mixed_pages_complete"] = checks["mixed_identity"] and valid_distribution(mixed.get("workloads", {}).get("page_during_import"), repetitions * 2)
    # Mixed RSS is diagnostic; the approved 4 GiB cap applies to warm/fresh browsing.
    checks["mixed_rss_recorded"] = type(mixed.get("peak_rss_bytes")) is int and mixed["peak_rss_bytes"] > 0
    # Report page-under-import p95 separately; do not invent a new mixed-page threshold.
    recovery = result.get("recovery", {})
    checks["recovery"] = valid_envelope(recovery, expected_identity, request("recovery", count)) and all(
        recovery.get("result", {}).get(stage, {}).get("expected") == expected
        and recovery["result"][stage].get("actual") == expected
        and type(recovery["result"][stage].get("forced_exit_code")) is int
        and recovery["result"][stage]["forced_exit_code"] != 0
        for stage, expected in (("before", 1), ("after", 2)))
    checks["copy_integrity"] = result.get("copy", {}).get("proof") == baseline["load"]["proof"] and bool(result.get("copy", {}).get("sha256"))
    return dict(checks=checks, all_pass=all(checks.values()), fresh_distributions=fresh_distributions,
                aggregate="not rerun; unchanged original evidence referenced",
                bounded_query_work="not evaluated", native_production_validation="still required")


def measure(db, count, output):
    def invoke(label, req):
        return child(db, req, output / (label + ".json"))
    result = {"plans": invoke("plans", request("plans", count)),
              "warm": invoke("warm", request("read", count, WARM_SAMPLES)), "fresh": {}}
    for name in PAGES:
        result["fresh"][name] = [invoke(f"fresh-{name}-{i}", request("read", count, 1, name, i)) for i in range(FRESH_SAMPLES)]
    # Every page workload finishes before any mutation of this scale's copy.
    result["mixed"] = invoke("mixed", request("mixed", count, WARM_SAMPLES * 2))
    result["recovery"] = invoke("recovery", request("recovery", count))
    return result


def run(snapshot, baseline_root, output):
    snapshot, baseline_root, output = (Path(p).resolve() for p in (snapshot, baseline_root, output))
    for source in (snapshot, baseline_root):
        require(not output.is_relative_to(source) and not source.is_relative_to(output), "output overlaps evidence source")
    require(not output.exists(), "experiment output already exists")
    current_identity = identity()
    snap = json.loads((snapshot / "snapshot.json").read_text())
    original = json.loads((baseline_root / "campaign.json").read_text())
    require(snap.get("complete") is True and snap.get("counts") == list(SCALES), "snapshot incomplete or wrong scales")
    require(original.get("complete") is True and original.get("counts") == list(SCALES), "baseline incomplete or wrong scales")
    require(original.get("repetitions") == WARM_SAMPLES and original.get("fresh_repetitions") == FRESH_SAMPLES, "baseline sample contract mismatch")
    require(original.get("host", {}).get("sqlite_version") == current_identity["engine_version"], "baseline native SQLite version mismatch")
    require(snap.get("frozen_harness_sha256") == original.get("script_sha256") == FROZEN_SHA256, "source SQL/harness identity mismatch")
    output.mkdir(parents=True, exist_ok=False)
    record = dict(version=VERSION, identity=current_identity, complete=False, all_pass=False,
        counts=list(SCALES), memory_mib=MEMORY_MIB, repetitions=WARM_SAMPLES, fresh_repetitions=FRESH_SAMPLES,
        snapshot_root=str(snapshot), baseline_root=str(baseline_root), snapshot_manifest_sha256=profiles.digest(snapshot / "snapshot.json"),
        baseline_manifest_sha256=profiles.digest(baseline_root / "campaign.json"), host=bench.host(), scales={})
    bench.dump(output / "experiment.json", record)
    for count in SCALES:
        directory = output / str(count)
        directory.mkdir()
        result = {}
        record["scales"][str(count)] = result
        try:
            baseline_path = baseline_root / str(count) / "sqlite.json"
            baseline = json.loads(baseline_path.read_text())
            require(baseline["warm_256mb"]["settings"] == current_identity["settings_requested"], "baseline connection settings mismatch")
            expected = snap["datasets"][str(count)]
            require(expected["count"] == count and expected["version"] == bench.VERSION and expected["seed"] == bench.SEED, "generator contract mismatch")
            require(baseline["load"]["proof"] == expected["expected"], "baseline loader proof mismatch")
            aggregate = baseline["warm_256mb"]["workloads"]["aggregate"]
            require(valid_distribution(aggregate["distribution"], WARM_SAMPLES), "original aggregate evidence incomplete")
            result["aggregate_reference"] = dict(receipt=str(baseline_path), receipt_sha256=profiles.digest(baseline_path),
                sql=BASE_SQL["aggregate"], distribution=aggregate["distribution"], measured=False,
                note="original 100-sample aggregate evidence; no candidate aggregate execution or new aggregate verdict")
            relative = Path(str(count)) / ENGINE / "catalog.sqlite3"
            source = snapshot / relative
            before = profiles.digest(source)
            require(before == snap["snapshots"][str(relative)]["sha256"], "pristine artifact digest mismatch")
            db = directory / "catalog.sqlite3"
            result["copy"] = profiles.copy_checkpointed(ENGINE, source, db, expected)
            require(profiles.digest(source) == before, "source changed while copying")
            result["source_sha256"] = before
            # SQLite backup may change physical header bytes. Its independent
            # target hash and generator proof identify the equivalent copy.
            result["measurement_host"] = bench.host()
            result.update(measure(db, count, directory))
            result["evaluation"] = evaluate(result, baseline, current_identity, count)
        except Exception as error:
            result["error"] = f"{type(error).__name__}: {error}"
        bench.dump(directory / "result.json", result)
        bench.dump(output / "experiment.json", record)
    record.update(complete=True, all_pass=all(value.get("evaluation", {}).get("all_pass") is True and "error" not in value for value in record["scales"].values()), host_after=bench.host())
    bench.dump(output / "experiment.json", record)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("run", "plans", "read", "mixed", "recovery", "crash-worker"))
    parser.add_argument("--snapshot", type=Path)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--db", type=Path)
    parser.add_argument("--count", type=int, default=0)
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument("--single", choices=PAGES)
    parser.add_argument("--iteration", type=int, default=0)
    parser.add_argument("--stage", choices=("before", "after"))
    parser.add_argument("--engine", choices=(ENGINE,), default=ENGINE)
    parser.add_argument("--memory-mb", type=int, choices=(MEMORY_MIB,), default=MEMORY_MIB)
    parser.add_argument("--trace-sql", action="store_true", help="small correctness tests only; never enabled by the experiment")
    args = parser.parse_args()
    if args.operation == "run":
        require(args.snapshot and args.baseline and args.output, "run requires snapshot/baseline/output")
        result = run(args.snapshot, args.baseline, args.output)
        return 0 if result["all_pass"] else 1
    require(args.db is not None and args.db.is_file() and args.repetitions > 0, "worker requires an existing database and positive repetitions")
    if args.operation == "crash-worker":
        require(args.stage is not None, "crash worker requires stage")
    else:
        require(args.count > 0, "worker requires positive count")
    result = worker(args)
    print(json.dumps(result, sort_keys=True))
    return 1 if "error" in result else 0


if __name__ == "__main__":
    raise SystemExit(main())
