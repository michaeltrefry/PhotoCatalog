#!/usr/bin/env python3
"""Fixed retained-service campaign; explicit reviewed binding and lane required.

Preparation/layout selection are separate. No retries, original reads or workload
stopping beyond this coordinator's own bounded children. All paths are private.
"""
from __future__ import annotations
import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import time

from preview_experiment import digest, exclusive, utc
from preview_host import HostObservation, host_identity


def anchor():
    return {"utc": utc(), "monotonic_ns": time.monotonic_ns()}


def read_json(path, maximum=64 * 1024 * 1024):
    with Path(path).open("rb") as stream:
        data = stream.read(maximum + 1)
    if len(data) > maximum:
        raise ValueError("receipt size limit exceeded")
    return json.loads(data)


def plan():
    return [(profile, workload, index) for profile in ("standard", "constrained")
            for workload, count in (("warm", 1), ("fresh", 20), ("navigation", 1))
            for index in range(count)]


def finite(value):
    if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
        raise ValueError("nonfinite, unavailable or negative measurement")
    return value


def distribution(values):
    ordered = sorted(finite(v) for v in values)
    if not ordered:
        raise ValueError("empty distribution")
    # Fixed nearest-rank percentiles; no interpolation or outlier deletion.
    return {"n": len(ordered), "p50": ordered[math.ceil(len(ordered)*.50)-1],
            "p95": ordered[math.ceil(len(ordered)*.95)-1],
            "p99": ordered[math.ceil(len(ordered)*.99)-1], "max": ordered[-1]}


def expected_trials(workload):
    if workload == "warm":
        return [("warmup", i) for i in range(3)] + [("warm", i) for i in range(100)] + [("hot_lru", 0)]
    if workload == "fresh":
        return [("fresh", 0)]
    if workload == "navigation":
        return [("navigation", i) for i in range(10)]
    raise ValueError("unknown fixed workload")


def validate_trial(row, kind, index):
    if row.get("complete") is not True or (row.get("kind"), row.get("index")) != (kind, index):
        raise ValueError("wrong/failed trial identity")
    finite(row.get("wall_ms"))
    peak = row.get("peak_resident_bytes")
    if type(peak) is not int or peak <= 0:
        raise ValueError("process high-water measurement unavailable")
    reads = row.get("reads", [])
    tickets = set()
    for read in reads:
        if read.get("outcome") != "ready":
            raise ValueError("required read failed; preserve its original classification")
        ticket = read.get("ticket")
        if type(ticket) is not int or ticket <= 0 or ticket in tickets:
            raise ValueError("duplicate/invalid completion identity")
        tickets.add(ticket)
        finite(read.get("queue_ms"))
        finite(read.get("owner_read_ms"))
        metrics = read.get("metrics", {})
        for field in ("catalog_identity_ms", "store_read_checksum_ms", "header_decode_ms", "total_ms"):
            finite(metrics.get(field))
        if metrics.get("returned_pixels") is not True:
            raise ValueError("read did not return owned pixels")
    if kind != "navigation":
        if len(reads) != 200 or {r.get("index") for r in reads} != set(range(200)) or row.get("verified_views") != 200:
            raise ValueError("page does not contain all 200 verified fixed identities")
        finite(row.get("verification_ms_outside_page"))
    else:
        viewports = row.get("viewports", [])
        if len(viewports) != 100 or [v.get("viewport") for v in viewports] != list(range(100)):
            raise ValueError("navigation trace incomplete/reordered")
        for v in viewports:
            if v.get("scheduled_ms") != v["viewport"] * 50:
                raise ValueError("trace clock shifted")
            finite(v.get("overrun_ms"))
        submitted, canceled = {}, set()
        for event in row.get("events", []):
            ticket = event.get("ticket")
            if event.get("action") == "submit":
                if ticket in submitted:
                    raise ValueError("ticket reused")
                submitted[ticket] = event.get("index")
            elif event.get("action") == "cancel":
                if submitted.get(ticket) != event.get("index") or ticket in canceled:
                    raise ValueError("unowned/duplicate cancellation")
                canceled.add(ticket)
            else:
                raise ValueError("unknown trace event")
        if tickets & canceled or tickets | canceled != set(submitted):
            raise ValueError("late or lost consumer completion")
        if any(submitted.get(r["ticket"]) != r.get("index") for r in reads):
            raise ValueError("completion identity differs from admitted request")
        finite(row.get("verification_ms_in_trace_wall"))
    return row


def validate_binding(binding, files):
    if binding.get("version") != 2 or binding.get("catalog_schema") != 6 or binding.get("clean") is not True:
        raise ValueError("clean reviewed source binding required")
    revision = binding.get("source_revision", "")
    if len(revision) != 40 or any(c not in "0123456789abcdef" for c in revision):
        raise ValueError("exact source revision required")
    if binding.get("planned_measured_children") != 44 or binding.get("planned_verifiers") != 44:
        raise ValueError("fixed child plan mismatch")
    for name, actual in files.items():
        if binding.get(name + "_sha256") != actual:
            raise ValueError(f"reviewed identity mismatch: {name}")


def run(args):
    if args.lane_token != "coordinator-authorized":
        raise ValueError("explicit coordinator lane required")
    args.output.mkdir(parents=False, exist_ok=False)
    root = Path(__file__).resolve().parents[1]
    campaign = {"version": 2, "catalog_schema": 6, "complete": False, "started": anchor(), "children": [],
                "planned_measured_children": 44, "planned_verifiers": 44,
                "automatic_retries": 0, "quietness_verified": False,
                "metadata_count": 10000, "desktop_frame_time": "unavailable; S12",
                "ten_million_integrated_rss_gate": "not awarded by this 10k component experiment"}
    error = None
    frozen = {}
    try:
        paths = {name: getattr(args, name) for name in ("binary", "worker", "archive", "storage", "fixture", "layout_receipt")}
        paths.update(protocol=root/"docs/PREVIEW_STAGE_B_PROTOCOL.md", coordinator=Path(__file__))
        fixture = read_json(args.fixture, 65536)
        paths["dataset"] = Path(fixture["dataset"])
        frozen = {name: digest(path) for name, path in paths.items()}
        validate_binding(read_json(args.binding, 65536), frozen)
        data = read_json(fixture["dataset"], 1024*1024)
        if fixture.get("count") != 10000 or data.get("count") != 10000 or Path(fixture["offline_originals"]).exists():
            raise ValueError("fixed 10k offline fixture required")
        layout = read_json(args.layout_receipt, 2*1024*1024)
        passes = layout.get("passes", [])
        if layout.get("complete") is not True or len(passes) != 6:
            raise ValueError("complete layout prerequisite required")
        if any(p.get("complete") is not True or p.get("lookup_count") != 10000 or p.get("distinct_actual_read_content_hashes") != 10000 for p in passes):
            raise ValueError("layout actual-payload cardinality not proven")
        if layout.get("dataset_blake3") != fixture.get("dataset_blake3"):
            raise ValueError("layout proof belongs to another dataset")
        campaign["identities"] = frozen
        campaign["binding_sha256"] = digest(args.binding)
        campaign["layout"] = data["store"]["layout"]
        campaign["host"] = host_identity(args.output, [args.fixture, Path(fixture["dataset"])])
        exclusive(args.output/"preflight.json", campaign)
        with HostObservation(args.output) as observer:
            for profile, workload, index in plan():
                name = f"{profile}-{workload}-{index:02}"
                folder = args.output/name
                child = {"name": name, "profile": profile, "workload": workload,
                         "index": index, "complete": False, "started": anchor()}
                campaign["children"].append(child)
                command = [str(args.binary), "run", "--fixture", str(args.fixture), "--worker", str(args.worker),
                           "--output", str(folder), "--profile", profile, "--workload", workload]
                with (args.output/f"{name}.stdout").open("xb") as out, (args.output/f"{name}.stderr").open("xb") as err:
                    process = subprocess.Popen(command, stdout=out, stderr=err, env={**os.environ, "OMP_NUM_THREADS": "1"})
                    child["pid"] = process.pid
                    try:
                        child["returncode"] = process.wait(timeout=900)
                    finally:
                        if process.poll() is None:
                            process.kill()
                            process.wait()
                        child["finished"] = anchor()
                if (folder/"receipt.json").exists():
                    child["receipt_sha256"] = digest(folder/"receipt.json")
                    child["result"] = read_json(folder/"receipt.json", 1024*1024)
                if child["returncode"] != 0:
                    raise ValueError(f"measured child failed: {name}")
                result = child["result"]
                if result.get("version") != 2 or result.get("catalog_schema") != 6 or result.get("complete") is not True or result.get("profile") != profile or result.get("workload") != workload:
                    raise ValueError("child result identity mismatch")
                if result.get("dataset_blake3") != fixture["dataset_blake3"]:
                    raise ValueError("child dataset identity mismatch")
                child["verification_started"] = anchor()
                with (folder/"verification.stdout").open("xb") as out, (folder/"verification.stderr").open("xb") as err:
                    verified = subprocess.run([str(args.binary), "verify", str(folder)], stdout=out, stderr=err, timeout=60, check=False)
                child["verification_finished"] = anchor()
                child["verification_returncode"] = verified.returncode
                if verified.returncode != 0 or read_json(folder/"verification.stdout").get("complete") is not True:
                    raise ValueError("trial receipt chain verification failed")
                trials = result.get("trials", [])
                expected = expected_trials(workload)
                if len(trials) != len(expected):
                    raise ValueError("fixed sample count mismatch")
                child["observations"] = []
                for entry, (kind, sample) in zip(trials, expected):
                    filename = f"{kind}-{sample:03}.json"
                    if entry.get("path") != filename:
                        raise ValueError("unexpected trial path")
                    row = validate_trial(read_json(folder/filename), kind, sample)
                    child["observations"].append({"kind":kind,"index":sample,"sha256":digest(folder/filename),
                        "wall_ms":row["wall_ms"],"peak_resident_bytes":row["peak_resident_bytes"],
                        "queue_ms":distribution([r["queue_ms"] for r in row["reads"]]),
                        "owner_read_ms":distribution([r["owner_read_ms"] for r in row["reads"]])})
                child["complete"] = True
                exclusive(args.output/f"{name}-child.json", child)
            campaign["telemetry"] = observer.finish()
            if campaign["telemetry"].get("complete") is not True:
                raise ValueError("host telemetry incomplete")
        campaign["profiles"] = {}
        for profile in ("standard", "constrained"):
            children = [c for c in campaign["children"] if c["profile"] == profile]
            summary = {}
            for kind in ("warmup", "warm", "hot_lru", "fresh", "navigation"):
                rows = [r for c in children for r in c["observations"] if r["kind"] == kind]
                summary[kind] = {"wall_ms":distribution([r["wall_ms"] for r in rows]),
                                 "peak_resident_bytes":max(r["peak_resident_bytes"] for r in rows)}
            summary["warm_headless_component_p95_within_1000ms"] = summary["warm"]["wall_ms"]["p95"] <= 1000
            summary["component_rss_within_4gib"] = max(s["peak_resident_bytes"] for s in summary.values() if isinstance(s, dict)) <= 4*1024**3
            summary["full_ui_and_10m_rss_acceptance"] = "not awarded by this component campaign"
            campaign["profiles"][profile] = summary
        campaign["complete"] = True
    except BaseException as exc:
        error = exc
        campaign["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        if frozen:
            try:
                campaign["bound_files_preserved"] = all(digest(paths[name]) == value for name, value in frozen.items())
            except OSError:
                campaign["bound_files_preserved"] = False
            if not campaign["bound_files_preserved"]:
                campaign["complete"] = False
                error = error or ValueError("bound files changed during campaign")
                campaign.setdefault("error", str(error))
        campaign["finished"] = anchor()
        exclusive(args.output/"campaign.json", campaign)
    if error is not None:
        raise error


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    for name in ("binary", "worker", "archive", "storage", "fixture", "layout-receipt", "binding", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--lane-token", required=True)
    run(parser.parse_args())
