#!/usr/bin/env python3
"""Prospective S12 instrumented frame-work evaluator; physical presentation stays separate."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path


FRAME = "timeline-record-type-rendering-frame"
SCRIPT = "timeline-record-type-script"
LAYOUT = "timeline-record-type-layout"
NETWORK = "timeline-record-type-network"
SCRIPT_WORK = {
    "script-evaluated", "api-script-evaluated", "microtask-dispatched",
    "event-dispatched", "observer-callback", "timer-fired", "animation-frame-fired",
}
LAYOUT_WORK = {"recalculate-styles", "forced-layout", "layout", "paint", "composite"}
SCRIPT_INSTANTS = {
    "timer-installed", "timer-removed", "animation-frame-requested",
    "animation-frame-canceled", "probe-sample-recorded",
}
LAYOUT_INSTANTS = {
    "invalidate-styles", "invalidate-layout", "first-contentful-paint",
    "largest-contentful-paint",
}
PROFILE_ENVELOPE = "console-profile-recorded"
GC = "garbage-collected"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def finite(value):
    return type(value) in (int, float) and math.isfinite(value)


def exact_number(value, expected):
    return finite(value) and value == expected


def sha256_text(value):
    return isinstance(value, str) and len(value) == 64 and all(character in "0123456789abcdef" for character in value)


def git_oid40(value):
    return isinstance(value, str) and len(value) == 40 and all(character in "0123456789abcdef" for character in value)


def span(row):
    start, end = row.get("startTime"), row.get("endTime")
    require(finite(start) and finite(end) and end >= start, "Malformed record interval")
    return float(start), float(end)


def intersect(interval, window):
    start, end = max(interval[0], window[0]), min(interval[1], window[1])
    return (start, end) if end > start else None


def union_length(intervals, window=None):
    clipped = []
    for interval in intervals:
        value = intersect(interval, window) if window else interval
        if value:
            clipped.append(value)
    if not clipped:
        return 0.0
    clipped.sort()
    total = 0.0
    start, end = clipped[0]
    for next_start, next_end in clipped[1:]:
        if next_start <= end:
            end = max(end, next_end)
        else:
            total += end - start
            start, end = next_start, next_end
    return total + end - start


def numpy_linear_percentile(values, percentile):
    """NumPy's default/`linear` method (Hyndman-Fan type 7), without a runtime dependency."""
    ordered = sorted(values)
    require(bool(ordered) and 0 <= percentile <= 100, "Invalid percentile input")
    index = (len(ordered) - 1) * percentile / 100
    lower, upper = math.floor(index), math.ceil(index)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (index - lower)


def percentile_metrics(values, budget_ms):
    require(bool(values) and all(finite(value) and value >= 0 for value in values), "Invalid metric values")
    result = {
        "method": "numpy_linear",
        "count": len(values),
        "p95_ms": numpy_linear_percentile(values, 95),
        "p99_ms": numpy_linear_percentile(values, 99),
        "max_ms": max(values),
        "over_budget": sum(value > budget_ms for value in values),
        "budget_ms": budget_ms,
    }
    result["passes_budget"] = result["p95_ms"] <= budget_ms
    return result


def marker_time(trace, name):
    matches = [marker for marker in trace["recording"]["markers"]
               if marker.get("type") == "timestamp" and marker.get("details") == name]
    require(len(matches) == 1 and finite(matches[0].get("time")), f"Missing/duplicate marker: {name}")
    return float(matches[0]["time"])


def trace_records(trace):
    recording = trace["recording"]
    require(trace.get("version") == 1 and recording.get("discontinuities") == [], "Trace version/discontinuity")
    require(recording.get("memoryPressureEvents") == [], "Memory pressure during trace")
    require(recording.get("instrumentTypes") == [
        "timeline-record-type-layout", "timeline-record-type-script", "timeline-record-type-rendering-frame"
    ], "Unexpected trace instruments")
    start, end = recording.get("startTime"), recording.get("endTime")
    require(finite(start) and finite(end) and start < end, "Invalid recording bounds")
    frames, task_rows, task_intervals, gc_rows, gc_intervals, profiles = [], [], [], [], [], []
    targets = set()
    event_counts = {}
    for row in recording["records"]:
        kind = row.get("type")
        if kind == NETWORK:
            continue
        require(kind in (FRAME, SCRIPT, LAYOUT), "Unknown timeline record type")
        row_start, row_end = span(row)
        require(start <= row_start <= row_end <= end, "Record outside recording bounds")
        if kind == FRAME:
            require(set(row) == {"type", "startTime", "endTime"}, "Unexpected rendering-frame fields")
            frames.append((row_start, row_end))
            continue
        event = row.get("eventType")
        require(isinstance(event, str) and event, "Missing event kind")
        event_counts[event] = event_counts.get(event, 0) + 1
        if kind == SCRIPT:
            target = row.get("target")
            require(isinstance(target, dict) and set(target) == {"identifier", "name", "type"}, "Missing/ambiguous script target")
            target_tuple = (target["identifier"], target["name"], target["type"])
            require(all(isinstance(value, str) and value for value in target_tuple), "Invalid script target")
            targets.add(target_tuple)
            allowed = SCRIPT_WORK | SCRIPT_INSTANTS | {PROFILE_ENVELOPE, GC}
            require(event in allowed, f"Unknown script event kind: {event}")
            if event in SCRIPT_INSTANTS:
                require(row_end == row_start, f"Positive nonexecution script marker: {event}")
            elif event == PROFILE_ENVELOPE:
                profiles.append((row_start, row_end))
            elif event == GC:
                if row_end > row_start:
                    gc_rows.append((event, row_start, row_end))
                    gc_intervals.append((row_start, row_end))
            elif row_end > row_start:
                task_rows.append((event, row_start, row_end))
                task_intervals.append((row_start, row_end))
        else:
            allowed = LAYOUT_WORK | LAYOUT_INSTANTS
            require(event in allowed, f"Unknown layout event kind: {event}")
            if event in LAYOUT_INSTANTS:
                require(row_end == row_start, f"Positive nonexecution layout marker: {event}")
            elif row_end > row_start:
                task_rows.append((event, row_start, row_end))
                task_intervals.append((row_start, row_end))
    require(len(targets) == 1, "Trace does not bind one page target")
    target = next(iter(targets))
    require(target[1:] == ("Page", "page"), "Trace target is not the inspected page")
    require(len(frames) >= 3, "Insufficient rendering frames")
    frames.sort()
    require(all(left[1] <= right[0] for left, right in zip(frames, frames[1:])), "Overlapping/unordered rendering frames")
    return {
        "recording_bounds": (float(start), float(end)), "frames": frames,
        "task_rows": task_rows, "tasks": task_intervals,
        "gc_rows": gc_rows, "gc": gc_intervals,
        "profile_envelopes": profiles, "target": {"identifier": target[0], "name": target[1], "type": target[2]},
        "event_counts": event_counts,
    }


def select_frames(parsed, start, end):
    require(start < end, "Reversed marker interval")
    frames = parsed["frames"]
    selected = [index for index, frame in enumerate(frames) if frame[0] <= end and frame[1] >= start]
    require(bool(selected) and selected == list(range(selected[0], selected[-1] + 1)), "Missing/noncontiguous frame cohort")
    require(selected[0] > 0 and selected[-1] + 1 < len(frames), "Missing boundary guard frame")
    recording_start, recording_end = parsed["recording_bounds"]
    require(recording_start <= frames[selected[0] - 1][0] and frames[selected[-1] + 1][1] <= recording_end, "Truncated guard frame")
    return selected


def analyze_frames(parsed, selected, budget_ms):
    frames, tasks, gc = parsed["frames"], parsed["tasks"], parsed["gc"]
    combined = tasks + gc
    rows = []
    for index in selected:
        frame = frames[index]
        assignment = (frames[index - 1][1], frames[index + 1][0])
        require(assignment[0] <= frame[0] <= frame[1] <= assignment[1], "Invalid assignment window")
        raw_task = union_length(tasks, frame)
        raw_gc = union_length(combined, frame)
        gate_task = union_length(tasks, assignment)
        gate_gc = union_length(combined, assignment)
        envelope = frame[1] - frame[0]
        require(raw_task <= raw_gc <= envelope + 1e-9 and raw_gc <= gate_gc + 1e-9, "Union exceeds containing window")
        rows.append({
            "frame_index": index,
            "start_s": frame[0], "end_s": frame[1],
            "assignment_start_s": assignment[0], "assignment_end_s": assignment[1],
            "elapsed_envelope_ms": envelope * 1000,
            "raw_task_work_ms": raw_task * 1000,
            "raw_page_gc_increment_ms": (raw_gc - raw_task) * 1000,
            "raw_task_plus_gc_ms": raw_gc * 1000,
            "unknown_uninstrumented_remainder_ms": max(0.0, envelope - raw_gc) * 1000,
            "assigned_task_work_ms": gate_task * 1000,
            "assigned_page_gc_increment_ms": (gate_gc - gate_task) * 1000,
            "assigned_task_plus_gc_ms": gate_gc * 1000,
            "assigned_gap_contribution_ms": (gate_gc - raw_gc) * 1000,
        })
    first, last = selected[0], selected[-1]
    gaps = [(frames[first - 1][1], frames[first][0])]
    gaps += [(frames[index][1], frames[index + 1][0]) for index in range(first, last)]
    gaps += [(frames[last][1], frames[last + 1][0])]
    unique_gap = sum(union_length(combined, gap) for gap in gaps)
    summed_gap = sum(row["assigned_gap_contribution_ms"] for row in rows) / 1000
    accounting = (frames[first - 1][1], frames[last + 1][0])
    relevant = [interval for interval in combined if intersect(interval, accounting)]
    require(all(any(intersect(interval, (frames[index - 1][1], frames[index + 1][0])) for index in selected)
                for interval in relevant), "Unassigned positive work interval")
    return {
        "frames": rows,
        "elapsed_envelope": percentile_metrics([row["elapsed_envelope_ms"] for row in rows], budget_ms),
        "raw_instrumented_task": percentile_metrics([row["raw_task_work_ms"] for row in rows], budget_ms),
        "raw_task_plus_page_gc": percentile_metrics([row["raw_task_plus_gc_ms"] for row in rows], budget_ms),
        "assigned_instrumented_task": percentile_metrics([row["assigned_task_work_ms"] for row in rows], budget_ms),
        "assigned_task_plus_page_gc_gate": percentile_metrics([row["assigned_task_plus_gc_ms"] for row in rows], budget_ms),
        "gap_accounting": {
            "gap_count": len(gaps),
            "positive_task_records": sum(any(intersect((start, end), gap) for gap in gaps) for _, start, end in parsed["task_rows"]),
            "positive_gc_records": sum(any(intersect((start, end), gap) for gap in gaps) for _, start, end in parsed["gc_rows"]),
            "unique_task_plus_gc_ms": unique_gap * 1000,
            "assigned_sum_ms": summed_gap * 1000,
            "deliberately_duplicated_ms": max(0.0, summed_gap - unique_gap) * 1000,
            "positive_records_in_accounting_bounds": len(relevant),
            "unassigned_positive_records": 0,
        },
        "accounting_bounds_s": list(accounting),
    }


def validate_conformance(trace, plan, package_binding, package_sha256, evaluator_sha256):
    require(plan["protocol"] == "s12_frame_work_v2_conformance_plan", "Wrong conformance protocol")
    require(isinstance(plan.get("run_id"), str) and plan["run_id"], "Missing conformance run ID")
    require(sha256_text(package_sha256) and sha256_text(evaluator_sha256)
            and plan["source_commit"] == package_binding["source_commit"] and plan["package_binding_sha256"] == package_sha256,
            "Conformance source/package binding mismatch")
    require(git_oid40(plan["source_commit"]) and package_binding.get("features") == ["measurement-devtools"]
            and sha256_text(package_binding["files"]["executable"]["sha256"]), "Unqualified conformance package binding")
    require(plan.get("evaluator_sha256") == evaluator_sha256, "Conformance evaluator binding mismatch")
    require(plan["gap_policy"] == "both_adjacent_frames" and plan["gc_policy"] == "gate_union_page_gc"
            and plan["unknown_event_policy"] == "fail", "Conformance policy changed")
    callbacks = plan["callbacks"]
    require(callbacks == {"total": 120, "idle": [10, 25], "task": 40, "layout_start": 60, "layout_end": 62}
            and all(type(value) is int for value in (callbacks["total"], *callbacks["idle"], callbacks["task"],
                                                     callbacks["layout_start"], callbacks["layout_end"]))
            and exact_number(plan["task_requested_ms"], 24) and exact_number(plan["marker_tolerance_ms"], 1)
            and exact_number(plan["idle_min_frames"], 5) and exact_number(plan["idle_max_gate_work_ms"], 5),
            "Conformance controls changed")
    require(plan["required_layout_kinds"] == ["forced-layout", "paint", "composite"]
            and plan["required_script_kind"] == "animation-frame-fired", "Conformance event requirements changed")
    installed = plan["installed"]
    require(installed == {"webkit_version": "21624.5.1.11.3",
                          "main_js_sha256": "8f52d98cb9bde071ba2e63719372da37d00c07122e6096a3a874ac969ca76b2f"},
            "Unqualified installed Inspector")
    parsed = trace_records(trace)
    names = plan["markers"]
    require(names == {
        "idle_start": "LensWorks:work-v2:idle:start", "idle_end": "LensWorks:work-v2:idle:end",
        "task_start": "LensWorks:work-v2:task:start", "task_end": "LensWorks:work-v2:task:end",
        "layout_start": "LensWorks:work-v2:layout:start", "layout_end": "LensWorks:work-v2:layout:end",
        "done": "LensWorks:work-v2:done",
    }, "Conformance markers changed")
    times = {name: marker_time(trace, value) for name, value in names.items()}
    require(times["idle_start"] < times["idle_end"] < times["task_start"] < times["task_end"]
            < times["layout_start"] < times["layout_end"] < times["done"], "Conformance marker order")
    idle_selected = select_frames(parsed, times["idle_start"], times["idle_end"])
    idle = analyze_frames(parsed, idle_selected, 16.7)
    idle_full_frames = [index for index in idle_selected
                        if parsed["frames"][index][0] >= times["idle_start"]
                        and parsed["frames"][index][1] <= times["idle_end"]]
    require(len(idle_full_frames) >= plan["idle_min_frames"], "Insufficient full idle conformance frames")
    idle_max = idle["assigned_task_plus_page_gc_gate"]["max_ms"]
    require(idle_max <= plan["idle_max_gate_work_ms"], "Idle phase inherited refresh envelope")
    tolerance = plan["marker_tolerance_ms"] / 1000
    task_marker = times["task_end"] - times["task_start"]
    require(abs(task_marker * 1000 - plan["task_requested_ms"]) <= plan["marker_tolerance_ms"], "Busy marker duration mismatch")
    script_intervals = [(start, end) for event, start, end in parsed["task_rows"] if event == "animation-frame-fired"]
    covered = union_length(script_intervals, (times["task_start"], times["task_end"]))
    require(any(start <= times["task_start"] + tolerance and end + tolerance >= times["task_end"]
                for start, end in script_intervals), "Busy marker lacks one containing script interval")
    require(covered + tolerance >= task_marker, "Busy marker not contained by script work")
    busy_selected = select_frames(parsed, times["task_start"], times["task_end"])
    busy = analyze_frames(parsed, busy_selected, 16.7)
    busy_gate_coverage = max(
        union_length(parsed["tasks"] + parsed["gc"], (times["task_start"], times["task_end"]))
        if parsed["frames"][index - 1][1] <= times["task_start"]
        and parsed["frames"][index + 1][0] >= times["task_end"] else 0
        for index in busy_selected
    )
    require(busy_gate_coverage + tolerance >= task_marker,
            "Busy frame assignment lost marked task work")
    layout_window = (times["layout_start"], times["layout_end"])
    layout_rows = [(event, start, end) for event, start, end in parsed["task_rows"] if intersect((start, end), layout_window)]
    kinds = {event for event, _, _ in layout_rows}
    require(set(plan["required_layout_kinds"]) <= kinds and plan["required_script_kind"] in kinds,
            "Missing nested layout/render conformance kind")
    clipped = [intersect((start, end), layout_window) for _, start, end in layout_rows]
    clipped = [value for value in clipped if value]
    simple_sum = sum(end - start for start, end in clipped)
    united = union_length(clipped)
    require(simple_sum > united + 1e-9, "Nested layout conformance did not exercise overlap union")
    animations = [(start, end) for event, start, end in layout_rows if event == "animation-frame-fired"]
    forced = [(start, end) for event, start, end in layout_rows if event == "forced-layout"]
    require(any(animation_start <= times["layout_start"] <= forced_start
                and forced_end <= animation_end
                for animation_start, animation_end in animations for forced_start, forced_end in forced),
            "Forced layout is not nested in the declared rAF task")
    layout_selected = select_frames(parsed, times["layout_start"], times["layout_end"])
    layout_analysis = analyze_frames(parsed, layout_selected, 16.7)
    require(any(row["assigned_task_work_ms"] > 0 for row in layout_analysis["frames"]), "Layout frame assignment lost work")
    empty_frames = [frame for frame in parsed["frames"] if union_length(parsed["tasks"] + parsed["gc"], frame) == 0]
    phase_callbacks = sum(event == "animation-frame-fired" and intersect((start, end), (times["idle_start"], times["done"])) is not None
                          for event, start, end in parsed["task_rows"])
    expected_phase_callbacks = plan["callbacks"]["total"] - plan["callbacks"]["idle"][0] + 1
    require(phase_callbacks >= expected_phase_callbacks, "Incomplete conformance rAF phase")
    return {
        "qualified": True, "run_id": plan["run_id"], "target": parsed["target"],
        "idle_frame_count": len(idle_selected), "idle_full_frame_count": len(idle_full_frames),
        "idle_max_gate_work_ms": idle_max,
        "task_marker_ms": task_marker * 1000, "task_marker_script_coverage_ms": covered * 1000,
        "task_marker_gate_coverage_ms": busy_gate_coverage * 1000,
        "layout_kinds": sorted(kinds), "layout_simple_sum_ms": simple_sum * 1000,
        "layout_union_ms": united * 1000, "busy_frame_count": len(busy_selected),
        "layout_frame_count": len(layout_selected), "phase_animation_frame_fired": phase_callbacks,
        "minimum_phase_animation_frame_fired": expected_phase_callbacks,
        "empty_work_frames": len(empty_frames),
    }


def validate_receipt(receipt, plan, marker_duration_ms):
    require(receipt["protocol"] == 2 and receipt["run_id"] == plan["run_id"] and receipt["overflowed"] == 0,
            "Receipt identity/overflow")
    capture = receipt["scroll_capture"]
    require(capture["outcome"] == "complete" and capture["reason"] == "duration_elapsed", "Incomplete scroll capture")
    frames = capture["frames"]
    require(2 <= len(frames) <= 2048 and all(isinstance(row, list) and len(row) == 3 for row in frames), "Invalid rAF series")
    require(all(type(value) is int and 0 <= value <= 2**53 - 1 for row in frames for value in row), "Lossy/out-of-bounds rAF/scroll value")
    require(all(left[0] < right[0] for left, right in zip(frames, frames[1:])), "Reversed rAF series")
    started, ended = capture["started_us"], capture["ended_us"]
    require(type(started) is int and type(ended) is int and 0 <= started < ended, "Invalid receipt bounds")
    require(abs((ended - started) / 1000 - plan["duration_ms"]) <= plan["duration_tolerance_ms"], "Unexpected receipt duration")
    require(abs(marker_duration_ms - (ended - started) / 1000) <= plan["marker_receipt_tolerance_ms"], "Marker/receipt duration mismatch")
    require(abs(frames[0][0] - started) <= plan["raf_edge_tolerance_us"]
            and 0 <= ended - frames[-1][0] <= plan["raf_edge_tolerance_us"], "Incomplete rAF edges")
    initial, final = capture["target_initial"], capture["target_final"]
    fields = ("identity", "scroll_top_px", "scroll_left_px", "viewport_width_px", "viewport_height_px", "scroll_width_px", "scroll_height_px")
    require(all(type(value.get(name)) is int and 0 <= value[name] <= 2**53 - 1 for value in (initial, final) for name in fields),
            "Invalid scroll target value")
    require(initial["identity"] == final["identity"] and all(initial[name] == final[name] for name in
            ("viewport_width_px", "viewport_height_px", "scroll_width_px", "scroll_height_px")), "Scroll target changed")
    require([initial["viewport_width_px"], initial["viewport_height_px"]] == plan["grid_viewport_css_pixels"], "Grid viewport mismatch")
    movement = abs(final["scroll_top_px"] - initial["scroll_top_px"]) + abs(final["scroll_left_px"] - initial["scroll_left_px"])
    require(movement >= plan["minimum_scroll_movement_px"] and len({(row[1], row[2]) for row in frames}) >= 2,
            "Insufficient scroll movement")
    intervals = [(right[0] - left[0]) / 1000 for left, right in zip(frames, frames[1:])]
    result = percentile_metrics(intervals, plan["budget_ms"])
    result["source_callback_count"] = len(frames)
    result["intervals_ms"] = intervals
    return result


def evaluate(trace, receipt, plan, context, package_binding, conformance_trace, conformance_plan,
             package_sha256, conformance_sha256, conformance_plan_sha256, evaluator_sha256):
    require(plan["protocol"] == "s12_frame_work_v2_plan" and plan["metric_version"] == 2, "Wrong frame-work plan")
    require(plan["percentile"] == "numpy_linear" and exact_number(plan["budget_ms"], 16.7), "Budget/percentile changed")
    require(plan["gap_policy"] == "both_adjacent_frames" and plan["gc_policy"] == "gate_union_page_gc"
            and plan["unknown_event_policy"] == "fail", "Metric policy changed")
    require(exact_number(plan["duration_ms"], 5000) and exact_number(plan["duration_tolerance_ms"], 1)
            and exact_number(plan["marker_receipt_tolerance_ms"], 1)
            and exact_number(plan["raf_edge_tolerance_us"], 100_000),
            "Capture duration/tolerances changed")
    require(type(plan["minimum_scroll_movement_px"]) is int and plan["minimum_scroll_movement_px"] > 0,
            "Invalid minimum scroll movement")
    require(all(isinstance(value, list) and len(value) == 2 and all(type(item) is int and item > 0 for item in value)
                for value in (plan["viewport_css_pixels"], plan["grid_viewport_css_pixels"])), "Invalid planned viewport")
    require(isinstance(plan["display_identity"], str) and plan["display_identity"]
            and isinstance(plan["competing_load"], str) and plan["competing_load"], "Missing display/competing-load declaration")
    require(sha256_text(package_sha256) and sha256_text(conformance_sha256)
            and sha256_text(conformance_plan_sha256) and sha256_text(evaluator_sha256)
            and plan["source_commit"] == package_binding["source_commit"] and plan["package_binding_sha256"] == package_sha256,
            "Source/package binding mismatch")
    require(git_oid40(plan["source_commit"]), "Invalid source commit")
    require(package_binding.get("features") == ["measurement-devtools"]
            and sha256_text(package_binding["files"]["executable"]["sha256"]), "Unqualified package binding")
    require(plan["conformance_trace_sha256"] == conformance_sha256
            and plan["conformance_plan_sha256"] == conformance_plan_sha256, "Conformance binding mismatch")
    require(plan.get("evaluator_sha256") == evaluator_sha256, "Measured evaluator binding mismatch")
    qualification = validate_conformance(conformance_trace, conformance_plan, package_binding, package_sha256, evaluator_sha256)
    require(context["protocol"] == "s12_frame_work_v2_context" and context["run_id"] == plan["run_id"], "Capture context identity")
    require(context["source_commit"] == plan["source_commit"]
            and context["executable_sha256"] == package_binding["files"]["executable"]["sha256"], "Capture executable/source binding")
    require(all(context.get(name) is True for name in
                ("focus_retained", "visibility_retained", "target_retained", "display_unchanged", "viewport_unchanged")),
            "Focus/visibility/target/display/viewport not retained")
    require(context["viewport_css_pixels"] == plan["viewport_css_pixels"]
            and context["grid_viewport_css_pixels"] == plan["grid_viewport_css_pixels"], "Capture viewport mismatch")
    require(context["display_identity"] == plan["display_identity"]
            and context["competing_load"] == plan["competing_load"], "Display/competing-load mismatch")
    parsed = trace_records(trace)
    require(context["trace_target"] == parsed["target"], "Capture page target mismatch")
    start_name, end_name = plan["markers"]["start"], plan["markers"]["end"]
    require(start_name == f"LensWorks:S12:{plan['run_id']}:scroll:start"
            and end_name == f"LensWorks:S12:{plan['run_id']}:scroll:end:duration_elapsed", "Unexpected scroll marker names")
    start, end = marker_time(trace, start_name), marker_time(trace, end_name)
    require(start < end and parsed["recording_bounds"][0] < start < end < parsed["recording_bounds"][1], "Marker/recording bounds")
    selected = select_frames(parsed, start, end)
    analysis = analyze_frames(parsed, selected, plan["budget_ms"])
    raf = validate_receipt(receipt, plan, (end - start) * 1000)
    gate = analysis["assigned_task_plus_page_gc_gate"]
    return {
        "protocol": "s12_frame_work_v2_result", "metric_version": 2,
        "run_id": plan["run_id"], "verdict": "FRAME_WORK_V2_PASS_REQUIRES_TERMINAL_ACCEPTANCE" if gate["p95_ms"] <= plan["budget_ms"] else "FAILED_FRAME_WORK",
        "scope": "Instrumented main-thread task wall intervals plus page-GC envelope; not CPU occupancy or physical presentation.",
        "target": parsed["target"], "marker_interval_s": [start, end],
        "qualification": qualification, "frame_work": analysis, "raf_dropped_opportunity_proxy": raf,
        "event_counts": parsed["event_counts"],
        "profile_envelopes": {"count": len(parsed["profile_envelopes"]),
                              "union_ms": union_length(parsed["profile_envelopes"]) * 1000},
        "evaluator_sha256": evaluator_sha256,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qualify-conformance", action="store_true")
    for name in ("trace", "receipt", "plan", "context", "package-binding", "conformance-trace", "conformance-plan"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    names = ("package-binding", "conformance-trace", "conformance-plan") if args.qualify_conformance else (
        "trace", "receipt", "plan", "context", "package-binding", "conformance-trace", "conformance-plan")
    missing = [name for name in names if getattr(args, name.replace("-", "_")) is None]
    if missing:
        parser.error("missing required arguments: " + ", ".join("--" + name for name in missing))
    paths = {name.replace("-", "_"): getattr(args, name.replace("-", "_")) for name in names}
    raw = {}
    result_protocol = "s12_frame_work_v2_conformance_result" if args.qualify_conformance else "s12_frame_work_v2_result"
    try:
        raw = {name: path.read_bytes() for name, path in paths.items()}
        values = {name: json.loads(value) for name, value in raw.items()}
        evaluator_sha256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        package_sha256 = hashlib.sha256(raw["package_binding"]).hexdigest()
        if args.qualify_conformance:
            qualification = validate_conformance(values["conformance_trace"], values["conformance_plan"],
                                                  values["package_binding"], package_sha256, evaluator_sha256)
            result = {"protocol": result_protocol, "metric_version": 2, "verdict": "QUALIFIED",
                      "qualification": qualification, "evaluator_sha256": evaluator_sha256}
        else:
            result = evaluate(values["trace"], values["receipt"], values["plan"], values["context"],
                              values["package_binding"], values["conformance_trace"], values["conformance_plan"],
                              package_sha256, hashlib.sha256(raw["conformance_trace"]).hexdigest(),
                              hashlib.sha256(raw["conformance_plan"]).hexdigest(), evaluator_sha256)
    except (ValueError, KeyError, TypeError, IndexError, OSError) as error:
        result = {"protocol": result_protocol, "metric_version": 2,
                  "verdict": "INVALID_EVIDENCE", "error": f"{type(error).__name__}: {error}"}
    result["inputs"] = {name: {"path": str(path), "sha256": hashlib.sha256(raw[name]).hexdigest() if name in raw else None}
                        for name, path in paths.items()}
    result["inputs"]["evaluator"] = {"path": str(Path(__file__).resolve()),
                                     "sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    with args.output.open("x") as output:
        json.dump(result, output, indent=2)
        output.write("\n")
    return 0 if result["verdict"] in ("QUALIFIED", "FRAME_WORK_V2_PASS_REQUIRES_TERMINAL_ACCEPTANCE") else 2


if __name__ == "__main__":
    raise SystemExit(main())
