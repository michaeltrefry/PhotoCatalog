#!/usr/bin/env python3
"""First Stage B gate: 30 serial full production workers, no retries or tuning.

Source-only until independent review and explicit coordinator lane grant. The
token is an accidental-execution guard, not proof of permission or host quietness.
"""
from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import time

from preview_experiment import digest, exclusive, utc, validate_manifest
from preview_host import HostObservation, host_identity


def anchor():
    return {"utc": utc(), "monotonic_ns": time.monotonic_ns()}


def worker_reservation(peaks):
    """Accounting margin fixed before collection, not a measured RSS bound."""
    if len(peaks) != 30 or any(type(v) is not int or v <= 0 for v in peaks):
        raise ValueError("all 30 positive process high-water measurements required")
    # 25 percent for corpus/platform variance plus 64 MiB for owner-side receipt,
    # validation and encoded buffers. This is a proposed admission reservation;
    # the coordinator must freeze it before concurrency or browsing experiments.
    mib = 1024 * 1024
    return math.ceil((max(peaks) * 1.25 + 64 * mib) / mib) * mib


def validate_result(value, item):
    if value.get("complete") is not True or value.get("fixture_id") != item["id"]:
        raise ValueError("worker failed or returned another fixture")
    if not value.get("source_blake3") or value["source_blake3"] != value.get("source_after_blake3"):
        raise ValueError("worker source preservation not proven")
    metadata = value.get("metadata", {})
    width, height = metadata.get("width"), metadata.get("height")
    if metadata.get("orientation") in (5, 6, 7, 8):
        width, height = height, width
    if (width, height) != (item["width"], item["height"]):
        raise ValueError("independent source dimensions mismatch")
    artifacts = value.get("artifacts", [])
    if len(artifacts) != 2 or {a["key"]["edge"] for a in artifacts} != {512, 1600}:
        raise ValueError("both selected tiers required")
    for artifact in artifacts:
        key = artifact["key"]
        if key["encoding"] != {"codec": "jpeg", "quality": 80}:
            raise ValueError("selected encoding mismatch")
        if key["renderer_version"] != value["renderer_identity"]:
            raise ValueError("renderer key mismatch")
        if not artifact.get("encoded_blake3") or not artifact.get("decoded_blake3"):
            raise ValueError("full production artifact verification missing")
        if artifact["bytes"] <= 0 or artifact["width"] <= 0 or artifact["height"] <= 0:
            raise ValueError("invalid object dimensions or bytes")
        if max(artifact["width"], artifact["height"]) > key["edge"]:
            raise ValueError("tier exceeds selected edge")
    peak = value.get("worker_peak_rss_bytes")
    if type(peak) is not int or peak <= 0 or not value.get("worker_peak_method"):
        raise ValueError("process high-water RSS unavailable")
    return peak


def run(args):
    if args.lane_token != "coordinator-authorized":
        raise ValueError("explicit coordinator grant required before execution")
    manifest = json.loads(args.manifest.read_text())
    validate_manifest(manifest)
    limits = json.loads(args.limits.read_text())
    if set(limits) != {"max_encoded_bytes", "max_intermediate_pixels", "max_allocation_bytes"}:
        raise ValueError("exact DecodeLimits fields required")
    if any(type(v) is not int or not 0 < v <= 2**64 - 1 for v in limits.values()):
        raise ValueError("invalid decode limits")
    args.output.mkdir(parents=False, exist_ok=False)
    root = Path(__file__).resolve().parents[1]
    sources = [Path(item["path"]) for item in manifest["inputs"]]
    campaign = {"version": 1, "complete": False, "started": anchor(),
                "phase": "preflight", "planned_probe_children": 30,
                "planned_native_grandchildren": 30, "planned_verification_children": 30, "children": [],
                "selected_pair": [{"edge": 512, "codec": "jpeg", "quality": 80},
                                  {"edge": 1600, "codec": "jpeg", "quality": 80}],
                "decode_limits": limits, "identities": {},
                "worker_poll_interval_ms": 5, "child_timeout_seconds": 360,
                "automatic_retries": 0, "quietness_verified": False,
                "reservation_formula": "ceil_MiB(max_child_high_water*1.25 + 64 MiB)",
                "reservation_scope": "proposed accounting allowance; not aggregate RSS enforcement"}
    error = None
    try:
        campaign["identities"] = {str(path): digest(path) for path in (
            args.manifest, args.limits, args.binary, args.worker, Path(__file__),
            root / "src/bin/preview_runtime_probe.rs", root / "src/preview/worker.rs",
            root / "docs/PREVIEW_STAGE_B_PROTOCOL.md", root / "Cargo.lock")}
        for item, source in zip(manifest["inputs"], sources):
            if digest(source) != item["sha256"]:
                raise ValueError(f"source digest mismatch: {item['id']}")
        campaign["host"] = host_identity(args.output, sources)
        exclusive(args.output / "preflight.json", campaign)
        with HostObservation(args.output) as observation:
            campaign["phase"] = "workers"
            for item in manifest["inputs"]:
                folder = args.output / item["id"]
                entry = {"id": item["id"], "started": anchor(), "complete": False}
                campaign["children"].append(entry)
                command = [str(args.binary), "worker", "--worker", str(args.worker), "--source", item["path"],
                           "--fixture-id", item["id"], "--limits", str(args.limits),
                           "--output", str(folder)]
                with (args.output / f"{item['id']}.stdout").open("xb") as out, \
                     (args.output / f"{item['id']}.stderr").open("xb") as err:
                    child = subprocess.Popen(command, stdout=out, stderr=err,
                                             env={**os.environ, "OMP_NUM_THREADS": "1"})
                    entry["pid"] = child.pid
                    try:
                        entry["returncode"] = child.wait(timeout=360)
                    finally:
                        if child.poll() is None:
                            child.kill()
                            child.wait()
                        entry["finished"] = anchor()
                path = folder / "receipt.json"
                if path.exists():
                    entry["receipt_sha256"] = digest(path)
                    entry["result"] = json.loads(path.read_text())
                if entry["returncode"] != 0:
                    raise ValueError(f"worker failed: {item['id']}")
                entry["peak_rss_bytes"] = validate_result(entry["result"], item)
                entry["verification_started"] = anchor()
                with (folder / "verification.stdout").open("xb") as out, (folder / "verification.stderr").open("xb") as err:
                    verified = subprocess.run([str(args.binary), "verify", str(folder)], stdout=out, stderr=err,
                                              check=False, timeout=30)
                entry["verification_finished"] = anchor()
                entry["verification_returncode"] = verified.returncode
                if verified.returncode != 0:
                    raise ValueError("saved artifact verification failed")
                verification = json.loads((folder / "verification.stdout").read_text())
                if verification.get("complete") is not True or verification.get("fixture_id") != item["id"]:
                    raise ValueError("wrong artifact verification receipt")
                if verification.get("renderer_identity") != entry["result"]["renderer_identity"]:
                    raise ValueError("artifact verifier renderer mismatch")
                entry["verification_sha256"] = digest(folder / "verification.stdout")
                entry["encoded_artifacts"] = []
                for artifact in entry["result"]["artifacts"]:
                    # Filenames come from the frozen two-tier contract, not a
                    # worker-supplied traversal path.
                    name = f"{artifact['key']['edge']}.jpg"
                    if artifact["path"] != name or (folder / name).stat().st_size != artifact["bytes"]:
                        raise ValueError("artifact path/size mismatch")
                    entry["encoded_artifacts"].append({"path": name, "sha256": digest(folder / name)})
                entry["complete"] = True
                exclusive(args.output / f"{item['id']}-child.json", entry)
            campaign["telemetry"] = observation.finish()
            if campaign["telemetry"].get("complete") is not True:
                raise ValueError("host telemetry is incomplete")
        campaign["phase"] = "source-recheck"
        campaign["source_preserved"] = all(digest(Path(item["path"])) == item["sha256"]
                                            for item in manifest["inputs"])
        if not campaign["source_preserved"]:
            raise ValueError("source changed during campaign")
        peaks = [entry["peak_rss_bytes"] for entry in campaign["children"]]
        campaign["maximum_worker_peak_rss_bytes"] = max(peaks)
        campaign["proposed_worker_reservation_bytes"] = worker_reservation(peaks)
        campaign["phase"] = "complete"
        campaign["complete"] = True
    except BaseException as exc:
        error = exc
        campaign["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        recheck = []
        for item in manifest["inputs"]:
            try:
                actual = digest(Path(item["path"]))
                recheck.append({"id": item["id"], "sha256": actual, "preserved": actual == item["sha256"]})
            except (OSError, ValueError) as exc:
                recheck.append({"id": item["id"], "preserved": False, "error": type(exc).__name__})
        campaign["final_sources"] = recheck
        campaign["source_preserved"] = all(item["preserved"] for item in recheck)
        if not campaign["source_preserved"]:
            campaign["complete"] = False
            if error is None:
                error = ValueError("source preservation failed during final verification")
                campaign["error"] = str(error)
        campaign["finished"] = anchor()
        exclusive(args.output / "campaign.json", campaign)
    if error is not None:
        raise error


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    for name in ("manifest", "limits", "binary", "worker", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--lane-token", required=True)
    run(parser.parse_args())
