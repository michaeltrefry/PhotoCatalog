#!/usr/bin/env python3
"""Prospective causal timing evaluation. Final export/integrity acceptance stays separate."""
import argparse
import json
from pathlib import Path

CLOCK = "macos_mach_absolute_ns"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def evaluate(receipt, observations, declaration):
    identity, summary = observations[0], observations[-1]
    require(identity.get("kind") == "identity" and summary.get("kind") == "summary", "Incomplete observer")
    require(identity.get("protocol") == summary.get("protocol") == 2 and identity.get("clock") == summary.get("clock") == CLOCK, "Unqualified observer clock")
    require(identity["job"] == declaration["job"] and identity["executable_sha256"] == declaration["executable_sha256"], "Observer binding mismatch")
    require(summary["fatal_errors"] == 0 and not any(row["kind"] == "error" for row in observations), "Observer errors")
    require(summary["root_same_birth"] and summary["executable_unchanged"], "Observer identity changed")
    require(receipt["protocol"] == 2 and receipt["run_id"] == declaration["run_id"] and receipt["overflowed"] == 0, "Receipt identity/overflow")
    expected = declaration["input_ordinals"]
    cutoff = declaration["setup_cutoff_ordinal"]
    require(type(cutoff) is int and 0 <= cutoff <= 312 and expected == list(range(cutoff + 1, cutoff + 201)), "Exactly 200 contiguous predeclared inputs required")
    samples = {sample["ordinal"]: sample for sample in receipt["samples"]}
    require(len(samples) == len(receipt["samples"]) and all(ordinal in samples and samples[ordinal]["kind"] == "edit" for ordinal in expected), "Missing/duplicate/wrong-kind input")
    require(set(samples) == set(range(1, cutoff + 201)), "Missing setup or undeclared post-setup input")
    alignment = receipt["clock_alignment"]
    require(alignment["model"] == "causal_native_brackets_v1" and alignment["interval_ms"] == 100 and alignment["duration_ms"] == 300_000, "Unqualified bridge")
    require(len(receipt["samples"]) <= 512, "Sample overflow")
    require(alignment["stop_reason"] in ("finalized", "duration_elapsed", "anchor_limit"), "Incomplete/failed bridge")
    require(len(alignment["anchors"]) <= 3002, "Anchor overflow")
    events, valid, prior_receive, prior_ns, session = set(), [], 0, 0, None
    def event(value):
        require(type(value) is int and 0 < value <= 7028 and value not in events, "Invalid/duplicate event")
        events.add(value)
    for index, anchor in enumerate(alignment["anchors"], 1):
        require(anchor["anchor_id"] == index and anchor["send_event"] > prior_receive, "Anchor sequence/overlap")
        event(anchor["send_event"])
        receive = anchor["receive_event"]
        if receive is None:
            require(index == len(alignment["anchors"]) and anchor["native"] is None and anchor["error"] == "incomplete", "Malformed pending anchor")
            continue
        event(receive)
        require(receive > anchor["send_event"], "Reversed anchor")
        prior_receive = receive
        require(anchor["error"] is None and anchor["native"] is not None, "Anchor error")
        native = anchor["native"]
        require(native["run_id"] == receipt["run_id"] and native["anchor_id"] == index and native["clock"] == CLOCK and native["native_pid"] == identity["root_pid"], "Native identity mismatch")
        require(isinstance(native["monotonic_ns"], str) and native["monotonic_ns"].isdigit() and len(native["monotonic_ns"]) <= 20, "Lossy native timestamp")
        ns = int(native["monotonic_ns"])
        require(0 < ns <= 2**64 - 1 and ns >= prior_ns, "Native clock reversed")
        require(bool(native["session_id"]) and (session is None or session == native["session_id"]), "Native session changed")
        session, prior_ns = native["session_id"], ns
        valid.append((anchor["send_event"], receive, ns))
    sample_events = {}
    for value in alignment["sample_events"]:
        require(value["ordinal"] not in sample_events and value["ordinal"] in samples, "Duplicate/unbound sample event")
        event(value["start_event"]); event(value["end_event"])
        require(value["start_event"] < value["end_event"], "Reversed sample")
        sample_events[value["ordinal"]] = value
    require(set(sample_events) == set(samples), "Missing sample events")
    raw_positive = [row for row in observations if row["kind"] in ("stage_admitted", "active")]
    raw_closes = [row for row in observations if row["kind"] == "segment_closed"]
    raw_gaps = [row for row in observations if row["kind"] == "observation_gap"]
    summary_ids = [segment["segment"] for segment in summary["segments"]]
    require(summary_ids == list(range(1, len(summary_ids) + 1)), "Summary segment IDs are not consecutive")
    require({row["segment"] for row in raw_positive} == set(summary_ids), "Raw/summary positive segment bijection mismatch")
    require(len(raw_closes) == len(summary_ids) and {row["segment"] for row in raw_closes} == set(summary_ids), "Raw/summary closure bijection mismatch")
    require(summary["positive_observations"] == len(raw_positive)
            and summary["admitted_stages"] == sum(row["kind"] == "stage_admitted" for row in observations) == len(summary_ids)
            and summary["observation_gaps"] == len(raw_gaps), "Raw/summary aggregate count mismatch")
    require(sum(row["kind"] == "identity" for row in observations) == sum(row["kind"] == "summary" for row in observations) == 1, "Repeated observer boundary")
    segments, workers = [], set()
    for segment in summary["segments"]:
        require(segment["stage"]["job"] == identity["job"], "Wrong segment job")
        key = (segment["pid"], segment["birth_unix_s"])
        require(key not in workers, "Reopened worker segment")
        workers.add(key)
        proof = [row for row in observations if row["kind"] in ("stage_admitted", "active") and row.get("segment") == segment["segment"]]
        require(bool(proof) and proof[0]["kind"] == "stage_admitted" and all(row["kind"] == "active" for row in proof[1:]), "Missing/reopened raw segment proof")
        require(len(proof) == segment["positive_observations"], "Raw positive count mismatch")
        for row in proof:
            require((row["pid"], row["birth_unix_s"]) == key and all(row[name] == segment["stage"][name] for name in ("job", "sequence", "attempt", "authority", "active_lock_device_inode")), "Raw segment identity mismatch")
            require(identity["monotonic_ns"] <= row["positive_before_monotonic_ns"] <= row["positive_after_monotonic_ns"] <= summary["measurement_end_monotonic_ns"], "Raw probe bounds")
        require(all(a["positive_after_monotonic_ns"] <= b["positive_before_monotonic_ns"] for a, b in zip(proof, proof[1:])), "Raw probe order")
        require(1 + sum(bool(row.get("lsof_rechecked")) for row in proof[1:]) == segment["lsof_confirmations"], "Raw lsof count mismatch")
        require(not any(row["kind"] in ("observation_gap", "stage_not_seen", "error") and row.get("pid") == segment["pid"]
                        and proof[0]["positive_before_monotonic_ns"] <= row["monotonic_ns"] <= proof[-1]["positive_after_monotonic_ns"] for row in observations), "Raw lifecycle gap inside segment")
        closed = next(row for row in raw_closes if row["segment"] == segment["segment"])
        closed_ns = segment["closed_observed_monotonic_ns"]
        require((closed["pid"], closed["birth_unix_s"]) == key and closed["closed_reason"] == segment["closed_reason"]
                and closed["monotonic_ns"] == closed_ns and proof[-1]["positive_after_monotonic_ns"] <= closed_ns <= summary["measurement_end_monotonic_ns"]
                and observations.index(closed) > observations.index(proof[-1]), "Raw/summary closure boundary mismatch")
        reason = segment["closed_reason"]
        if reason == "observer_end":
            require(closed_ns == summary["measurement_end_monotonic_ns"], "Observer end closure mismatch")
        else:
            def closes_for_reason(row):
                if row["kind"] == "stage_not_seen":
                    return reason == "child_not_seen"
                if row["kind"] != "observation_gap":
                    return False
                if reason == "denied_access":
                    return row.get("lifecycle") == "gone" and row.get("error", "").startswith("AccessDenied:")
                if reason == "lifecycle_race":
                    return row.get("reason", "").startswith(("NoSuchProcess:", "ZombieProcess:", "FileNotFoundError:"))
                return reason in ("pid_birth_not_current", "active_lock_not_contended", "pid_birth_changed_after_lsof",
                                  "active_lock_changed_after_lsof", "pid_birth_changed_after_probe") and row.get("reason") == reason
            require(any(closes_for_reason(row) and row.get("pid") == key[0] and row.get("birth_unix_s", key[1]) == key[1]
                        and closed_ns <= row["monotonic_ns"] <= summary["measurement_end_monotonic_ns"]
                        for row in observations[observations.index(closed) + 1:]), "Missing raw closure cause")
        lo, hi = segment["first_positive_after_monotonic_ns"], segment["last_positive_before_monotonic_ns"]
        require(lo == proof[0]["positive_after_monotonic_ns"] and hi == (proof[-1]["positive_before_monotonic_ns"] if len(proof) > 1 else None), "Summary/raw endpoint mismatch")
        if hi is None:
            continue
        require(identity["monotonic_ns"] <= lo <= hi <= summary["measurement_end_monotonic_ns"], "Segment bounds")
        require(segment["positive_observations"] >= 2 and segment["lsof_confirmations"] >= 1, "Insufficient native proof")
        segments.append((lo, hi, segment))
    require(summary["usable_segments"] == len(segments), "Raw/summary usable count mismatch")
    segments.sort(key=lambda value: value[0])
    require(all(a[1] < b[0] for a, b in zip(segments, segments[1:])), "Overlapping native workers")

    def envelope(start, end):
        before = [anchor for anchor in valid if anchor[1] < start]
        after = [anchor for anchor in valid if anchor[0] > end]
        if not before or not after:
            return None
        low, high = before[-1][2], after[0][2]
        for lo, hi, segment in segments:
            if lo <= low <= high <= hi:
                return {"segment": segment["segment"], "lower_ns": str(low), "upper_ns": str(high),
                        "sequence": segment["stage"]["sequence"], "attempt": segment["stage"]["attempt"], "authority": segment["stage"]["authority"]}
        return None

    # Freeze selection before accessing any durable latency value.
    selected, decisions, failures, unknown_failures = [], [], [], []
    for ordinal in expected:
        sample, ev = samples[ordinal], sample_events[ordinal]
        full = envelope(ev["start_event"], ev["end_event"])
        start = envelope(ev["start_event"], ev["start_event"])
        if sample["outcome"] != "complete":
            (failures if start else unknown_failures).append(ordinal)
        if full and len(selected) < 100:
            selected.append(ordinal)
        decisions.append({"ordinal": ordinal, "outcome": sample["outcome"], "complete_envelope": full, "start_envelope": start})
    result = {"protocol": 2, "run_id": receipt["run_id"], "cohort": selected, "decisions": decisions,
              "proven_active_failures": failures, "unproven_failures": unknown_failures,
              "scope": "Timing only; final job/item reconciliation, outputs and source invariance remain required."}
    if failures:
        return {**result, "verdict": "FAILED_ACTIVE_INPUT"}
    if len(selected) < 100 or unknown_failures:
        return {**result, "verdict": "PARTIAL"}
    require(all(samples[i]["outcome"] == "complete" and samples[i]["during_export"] and type(samples[i].get("durable_us")) is int
                and type(samples[i].get("presentation_us")) is int and 0 <= samples[i]["durable_us"] <= samples[i]["presentation_us"] for i in selected), "Invalid cohort outcome/timing")
    import numpy as np
    latency = [samples[i]["durable_us"] / 1000 for i in selected]
    p95 = float(np.percentile(latency, 95, method="linear"))
    return {**result, "durable_p95_ms": p95, "durable_max_ms": max(latency),
            "verdict": "TIMING_PASS_REQUIRES_EXPORT_RECONCILIATION" if p95 <= 100 else "FAILED_LATENCY"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("receipt", "observer", "declaration", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    args = parser.parse_args()
    try:
        result = evaluate(json.loads(args.receipt.read_text()), [json.loads(line) for line in args.observer.read_text().splitlines()], json.loads(args.declaration.read_text()))
    except (ValueError, KeyError, TypeError, IndexError) as error:
        result = {"verdict": "INVALID_EVIDENCE", "error": str(error)}
    with args.output.open("x") as output:
        json.dump(result, output, indent=2); output.write("\n")
    raise SystemExit(0 if result["verdict"] == "TIMING_PASS_REQUIRES_EXPORT_RECONCILIATION" else 2)
