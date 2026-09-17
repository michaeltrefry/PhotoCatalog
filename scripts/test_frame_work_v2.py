import copy
import unittest

from evaluate_frame_work_v2 import (
    analyze_frames, evaluate, numpy_linear_percentile, select_frames, trace_records, union_length,
    validate_conformance,
)


TARGET = {"identifier": "page-1", "name": "Page", "type": "page"}
PACKAGE_SHA = "b" * 64
CONFORMANCE_SHA = "c" * 64
CONFORMANCE_PLAN_SHA = "d" * 64
EVALUATOR_SHA = "f" * 64
PACKAGE = {"source_commit": "a" * 40, "features": ["measurement-devtools"],
           "files": {"executable": {"sha256": "e" * 64}}}


def frame(start, end):
    return {"type": "timeline-record-type-rendering-frame", "startTime": start, "endTime": end}


def script(event, start, end, target=TARGET):
    return {"type": "timeline-record-type-script", "eventType": event, "startTime": start, "endTime": end,
            "details": 1, "extraDetails": None, "target": target}


def layout(event, start, end):
    return {"type": "timeline-record-type-layout", "eventType": event, "startTime": start, "endTime": end}


def marker(name, time):
    return {"type": "timestamp", "details": name, "time": time}


def trace(records, markers=(), start=0.0, end=10.0):
    return {
        "version": 1,
        "overview": {},
        "recording": {
            "displayName": "fixture", "startTime": start, "endTime": end,
            "discontinuities": [],
            "instrumentTypes": ["timeline-record-type-layout", "timeline-record-type-script", "timeline-record-type-rendering-frame"],
            "records": records, "markers": list(markers), "memoryPressureEvents": [], "samples": [],
        },
    }


class IntervalTests(unittest.TestCase):
    def test_numpy_linear_percentile_matches_declared_interpolation(self):
        values = [0.0, 10.0, 20.0, 30.0]
        self.assertAlmostEqual(numpy_linear_percentile(values, 95), 28.5)
        self.assertAlmostEqual(numpy_linear_percentile(values, 99), 29.7)

    def test_union_clips_and_merges_nested_overlaps_once(self):
        values = [(0.0, 5.0), (1.0, 2.0), (4.0, 7.0), (8.0, 9.0)]
        self.assertAlmostEqual(union_length(values), 8.0)
        self.assertAlmostEqual(union_length(values, (1.5, 8.5)), 6.0)
        self.assertEqual(union_length([]), 0.0)

    def test_gap_work_is_assigned_to_both_adjacent_frames_and_reported(self):
        value = trace([
            frame(0.0, 1.0), frame(1.1, 2.0), frame(2.1, 3.0), frame(3.1, 4.0),
            script("microtask-dispatched", 2.02, 2.08),
        ], start=0.0, end=4.0)
        parsed = trace_records(value)
        result = analyze_frames(parsed, [1, 2], 16.7)
        self.assertAlmostEqual(result["frames"][0]["assigned_gap_contribution_ms"], 60.0)
        self.assertAlmostEqual(result["frames"][1]["assigned_gap_contribution_ms"], 60.0)
        self.assertAlmostEqual(result["gap_accounting"]["unique_task_plus_gc_ms"], 60.0)
        self.assertAlmostEqual(result["gap_accounting"]["deliberately_duplicated_ms"], 60.0)

    def test_cross_frame_work_is_clipped_per_window_without_dropping_it(self):
        value = trace([
            frame(0.0, 1.0), frame(1.1, 2.0), frame(2.1, 3.0), frame(3.1, 4.0),
            script("event-dispatched", 1.5, 2.5), layout("forced-layout", 1.7, 1.8),
        ], start=0.0, end=4.0)
        result = analyze_frames(trace_records(value), [1, 2], 16.7)
        self.assertAlmostEqual(result["frames"][0]["raw_task_work_ms"], 500.0)
        self.assertAlmostEqual(result["frames"][1]["raw_task_work_ms"], 400.0)
        self.assertAlmostEqual(result["frames"][0]["assigned_task_work_ms"], 600.0)
        self.assertAlmostEqual(result["frames"][1]["assigned_task_work_ms"], 500.0)

    def test_page_gc_is_separate_but_conservatively_included_in_gate(self):
        value = trace([
            frame(0.0, 1.0), frame(1.1, 2.0), frame(2.1, 3.0),
            script("animation-frame-fired", 1.2, 1.3), script("garbage-collected", 1.25, 1.4),
        ], start=0.0, end=3.0)
        result = analyze_frames(trace_records(value), [1], 16.7)["frames"][0]
        self.assertAlmostEqual(result["raw_task_work_ms"], 100.0)
        self.assertAlmostEqual(result["raw_page_gc_increment_ms"], 100.0)
        self.assertAlmostEqual(result["raw_task_plus_gc_ms"], 200.0)
        self.assertAlmostEqual(result["unknown_uninstrumented_remainder_ms"], 700.0)


class EvidenceTests(unittest.TestCase):
    def test_boundary_cohort_keeps_full_intersecting_frames_and_requires_guards(self):
        value = trace([frame(0, 1), frame(1.1, 2), frame(2.1, 3), frame(3.1, 4), script("timer-fired", 1.2, 1.3)], start=0, end=4)
        parsed = trace_records(value)
        self.assertEqual(select_frames(parsed, 1.5, 2.5), [1, 2])
        with self.assertRaisesRegex(ValueError, "guard"):
            select_frames(parsed, 0.5, 1.5)

    def test_unknown_event_kind_and_positive_instant_fail_closed(self):
        base = [frame(0, 1), frame(1.1, 2), frame(2.1, 3)]
        with self.assertRaisesRegex(ValueError, "Unknown script event kind"):
            trace_records(trace(base + [script("future-task", 1.2, 1.3)], start=0, end=3))
        with self.assertRaisesRegex(ValueError, "Positive nonexecution"):
            trace_records(trace(base + [script("timer-installed", 1.2, 1.3)], start=0, end=3))
        parsed = trace_records(trace(base + [script("console-profile-recorded", 1.2, 1.3), script("timer-fired", 1.4, 1.5)], start=0, end=3))
        self.assertEqual(parsed["profile_envelopes"], [(1.2, 1.3)])

    def test_unknown_timeline_record_type_fails_closed(self):
        records = [frame(0, 1), frame(1.1, 2), frame(2.1, 3), script("timer-fired", 1.2, 1.3),
                   {"type": "timeline-record-type-future", "startTime": 1.4, "endTime": 1.5}]
        with self.assertRaisesRegex(ValueError, "Unknown timeline record type"):
            trace_records(trace(records, start=0, end=3))

    def test_multiple_page_targets_fail_closed(self):
        other = {"identifier": "worker-1", "name": "Worker", "type": "worker"}
        records = [frame(0, 1), frame(1.1, 2), frame(2.1, 3), script("timer-fired", 1.2, 1.3), script("timer-fired", 1.4, 1.5, other)]
        with self.assertRaisesRegex(ValueError, "one page target"):
            trace_records(trace(records, start=0, end=3))

    def test_nonfinite_and_negative_intervals_fail_closed(self):
        base = [frame(0, 1), frame(1.1, 2), frame(2.1, 3)]
        with self.assertRaisesRegex(ValueError, "Malformed record interval"):
            trace_records(trace(base + [script("timer-fired", float("nan"), 1.3)], start=0, end=3))
        with self.assertRaisesRegex(ValueError, "Malformed record interval"):
            trace_records(trace(base + [script("timer-fired", 1.3, 1.2)], start=0, end=3))


def conformance_fixture():
    names = {
        "idle_start": "LensWorks:work-v2:idle:start", "idle_end": "LensWorks:work-v2:idle:end",
        "task_start": "LensWorks:work-v2:task:start", "task_end": "LensWorks:work-v2:task:end",
        "layout_start": "LensWorks:work-v2:layout:start", "layout_end": "LensWorks:work-v2:layout:end",
        "done": "LensWorks:work-v2:done",
    }
    frames = [frame(i * .25, i * .25 + .2) for i in range(40)]
    records = frames + [
        script("animation-frame-fired", 1.2, 1.2005),
        script("animation-frame-fired", 4.0, 4.2),
        script("animation-frame-fired", 6.0, 6.5),
        layout("forced-layout", 6.1, 6.2), layout("paint", 6.3, 6.35), layout("composite", 6.35, 6.4),
    ]
    records += [script("animation-frame-fired", 7.0 + index * .009, 7.0001 + index * .009) for index in range(117)]
    markers = [marker(names["idle_start"], 1.1), marker(names["idle_end"], 2.8),
               marker(names["task_start"], 4.1), marker(names["task_end"], 4.124),
               marker(names["layout_start"], 6.05), marker(names["layout_end"], 6.7), marker(names["done"], 8.2)]
    plan = {
        "protocol": "s12_frame_work_v2_conformance_plan", "run_id": "s12-v31-frame-work-01", "source_commit": "a" * 40,
        "package_binding_sha256": PACKAGE_SHA, "evaluator_sha256": EVALUATOR_SHA, "installed": {
            "webkit_version": "21624.5.1.11.3",
            "main_js_sha256": "8f52d98cb9bde071ba2e63719372da37d00c07122e6096a3a874ac969ca76b2f",
        }, "markers": names, "callbacks": {"total": 120, "idle": [10, 25], "task": 40, "layout_start": 60, "layout_end": 62}, "task_requested_ms": 24,
        "marker_tolerance_ms": 1, "idle_min_frames": 5, "idle_max_gate_work_ms": 5,
        "gap_policy": "both_adjacent_frames", "gc_policy": "gate_union_page_gc", "unknown_event_policy": "fail",
        "required_layout_kinds": ["forced-layout", "paint", "composite"],
        "required_script_kind": "animation-frame-fired",
    }
    return trace(records, markers), plan


class EvaluationTests(unittest.TestCase):
    def fixture(self):
        conformance, conformance_plan = conformance_fixture()
        start_name, end_name = "LensWorks:S12:run:scroll:start", "LensWorks:S12:run:scroll:end:duration_elapsed"
        measured = trace([
            frame(0, .9), frame(1, 1.9), frame(2, 2.9), frame(3, 3.9), frame(4, 4.9), frame(5, 5.9), frame(6, 6.9), frame(7, 7.9),
            script("animation-frame-fired", 1.2, 1.205), layout("composite", 1.206, 1.208),
            script("animation-frame-fired", 2.2, 2.205), layout("paint", 2.206, 2.208),
            script("animation-frame-fired", 3.2, 3.205),
        ], [marker(start_name, 1.1), marker(end_name, 6.1)], start=0, end=8)
        package = copy.deepcopy(PACKAGE)
        plan = {
            "protocol": "s12_frame_work_v2_plan", "metric_version": 2, "run_id": "run",
            "source_commit": "a" * 40, "package_binding_sha256": PACKAGE_SHA,
            "conformance_trace_sha256": CONFORMANCE_SHA, "conformance_plan_sha256": CONFORMANCE_PLAN_SHA,
            "evaluator_sha256": EVALUATOR_SHA,
            "percentile": "numpy_linear", "budget_ms": 16.7, "gap_policy": "both_adjacent_frames",
            "gc_policy": "gate_union_page_gc", "unknown_event_policy": "fail",
            "markers": {"start": start_name, "end": end_name}, "duration_ms": 5000,
            "duration_tolerance_ms": 1, "marker_receipt_tolerance_ms": 1, "raf_edge_tolerance_us": 100000,
            "grid_viewport_css_pixels": [800, 600], "viewport_css_pixels": [1200, 800],
            "minimum_scroll_movement_px": 100, "display_identity": "display", "competing_load": "declared-idle",
        }
        receipt = {
            "protocol": 2, "run_id": "run", "overflowed": 0,
            "scroll_capture": {"outcome": "complete", "reason": "duration_elapsed", "started_us": 1_100_000,
                "ended_us": 6_100_000, "frames": [[1_101_000, 0, 0], [3_000_000, 0, 0], [6_099_000, 200, 0]],
                "target_initial": {"identity": 1, "scroll_top_px": 0, "scroll_left_px": 0, "viewport_width_px": 800,
                    "viewport_height_px": 600, "scroll_width_px": 800, "scroll_height_px": 2000},
                "target_final": {"identity": 1, "scroll_top_px": 200, "scroll_left_px": 0, "viewport_width_px": 800,
                    "viewport_height_px": 600, "scroll_width_px": 800, "scroll_height_px": 2000}},
        }
        context = {"protocol": "s12_frame_work_v2_context", "run_id": "run", "source_commit": "a" * 40,
                   "executable_sha256": "e" * 64, "trace_target": TARGET, "focus_retained": True,
                   "visibility_retained": True, "target_retained": True, "display_unchanged": True,
                   "viewport_unchanged": True, "viewport_css_pixels": [1200, 800], "grid_viewport_css_pixels": [800, 600],
                   "display_identity": "display", "competing_load": "declared-idle"}
        return measured, receipt, plan, context, package, conformance, conformance_plan

    def test_qualified_complete_evidence_produces_versioned_result(self):
        values = self.fixture()
        result = evaluate(*values, PACKAGE_SHA, CONFORMANCE_SHA, CONFORMANCE_PLAN_SHA, EVALUATOR_SHA)
        self.assertEqual(result["protocol"], "s12_frame_work_v2_result")
        self.assertTrue(result["qualification"]["qualified"])
        self.assertEqual(result["frame_work"]["assigned_task_plus_page_gc_gate"]["count"], 6)
        self.assertEqual(result["raf_dropped_opportunity_proxy"]["source_callback_count"], 3)
        self.assertEqual(len(result["raf_dropped_opportunity_proxy"]["intervals_ms"]), 2)

    def test_focus_or_visibility_loss_invalidates_instead_of_shrinking_cohort(self):
        values = list(self.fixture())
        values[3] = copy.deepcopy(values[3])
        values[3]["focus_retained"] = False
        with self.assertRaisesRegex(ValueError, "Focus/visibility"):
            evaluate(*values, PACKAGE_SHA, CONFORMANCE_SHA, CONFORMANCE_PLAN_SHA, EVALUATOR_SHA)

    def test_unqualified_busy_control_blocks_measurement(self):
        values = list(self.fixture())
        values[5] = copy.deepcopy(values[5])
        for record in values[5]["recording"]["records"]:
            if record.get("eventType") == "animation-frame-fired" and record.get("startTime") == 4.0:
                record["endTime"] = 4.11
        with self.assertRaisesRegex(ValueError, "Busy marker"):
            evaluate(*values, PACKAGE_SHA, CONFORMANCE_SHA, CONFORMANCE_PLAN_SHA, EVALUATOR_SHA)

    def test_conformance_constants_cannot_be_relaxed_by_plan(self):
        conformance, plan = conformance_fixture()
        package = copy.deepcopy(PACKAGE)
        mutations = [
            ("task_requested_ms", .01), ("marker_tolerance_ms", 1000),
            ("idle_min_frames", 0), ("idle_max_gate_work_ms", 1_000_000),
        ]
        for field, value in mutations:
            changed = copy.deepcopy(plan)
            changed[field] = value
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "controls"):
                validate_conformance(conformance, changed, package, PACKAGE_SHA, EVALUATOR_SHA)
        changed = copy.deepcopy(plan)
        changed["callbacks"]["total"] = 0
        with self.assertRaisesRegex(ValueError, "controls"):
            validate_conformance(conformance, changed, package, PACKAGE_SHA, EVALUATOR_SHA)

    def test_conformance_requires_evaluator_binding(self):
        conformance, plan = conformance_fixture()
        del plan["evaluator_sha256"]
        with self.assertRaisesRegex(ValueError, "evaluator binding"):
            validate_conformance(conformance, plan, PACKAGE, PACKAGE_SHA, EVALUATOR_SHA)

    def test_busy_and_layout_controls_require_guarded_frame_cohorts(self):
        conformance, plan = conformance_fixture()
        package = copy.deepcopy(PACKAGE)
        for starts in ({4.0}, {6.0, 6.25, 6.5}):
            changed = copy.deepcopy(conformance)
            changed["recording"]["records"] = [
                row for row in changed["recording"]["records"]
                if not (row["type"] == "timeline-record-type-rendering-frame" and row["startTime"] in starts)
            ]
            with self.subTest(starts=starts), self.assertRaisesRegex(ValueError, "frame cohort"):
                validate_conformance(changed, plan, package, PACKAGE_SHA, EVALUATOR_SHA)

    def test_nested_forced_layout_must_belong_to_marked_raf(self):
        conformance, plan = conformance_fixture()
        changed = copy.deepcopy(conformance)
        for row in changed["recording"]["records"]:
            if row.get("eventType") == "animation-frame-fired" and row.get("startTime") == 6.0:
                row["startTime"], row["endTime"] = 6.2, 6.5
        with self.assertRaisesRegex(ValueError, "declared rAF"):
            validate_conformance(changed, plan, PACKAGE, PACKAGE_SHA, EVALUATOR_SHA)

    def test_conformance_allows_continuous_raf_with_no_literal_empty_frame(self):
        conformance, plan = conformance_fixture()
        for row in list(conformance["recording"]["records"]):
            if row["type"] == "timeline-record-type-rendering-frame":
                conformance["recording"]["records"].append(layout("composite", row["startTime"], row["startTime"] + .00001))
        result = validate_conformance(conformance, plan, PACKAGE, PACKAGE_SHA, EVALUATOR_SHA)
        self.assertEqual(result["empty_work_frames"], 0)

    def test_measured_duration_is_fixed_to_five_seconds(self):
        values = list(self.fixture())
        values[2] = copy.deepcopy(values[2])
        values[2]["duration_ms"] = 2700
        with self.assertRaisesRegex(ValueError, "duration/tolerances"):
            evaluate(*values, PACKAGE_SHA, CONFORMANCE_SHA, CONFORMANCE_PLAN_SHA, EVALUATOR_SHA)


if __name__ == "__main__":
    unittest.main()
