"""Bounded runner tests use disposable synthetic subprocesses, never Lightroom."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("lightroom_private_run", Path(__file__).parents[1]/"scripts/run_lightroom_inspection.py")
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


@unittest.skipUnless(os.name == "posix", "private Mac coordinator uses POSIX process groups and locking")
class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.source = self.root/"fixture-bin"
        self.source.write_text("""#!/usr/bin/env python3
import json,sys,time
from pathlib import Path
mode=sys.argv[2]
count=Path(sys.argv[3])
count.write_text(str(int(count.read_text())+1) if count.exists() else '1')
if mode=='flood':
 sys.stdout.write('x'*1000000)
elif mode=='sleep':
 time.sleep(120)
else:
 if mode=='pause':
  Path(sys.argv[4]).write_text('coordinator pause')
  time.sleep(0.1)
 print(json.dumps({'fixture':True}))
""")
        self.source.chmod(0o700)
        self.config = copy.deepcopy(RUN.DEFAULTS)
        self.config.update(tested_binary=str(self.source), catalog_root=str(self.root/"catalogs"), original_root=str(self.root/"originals"), exclusive_output=str(self.root/"run"), minimum_free_bytes=1)
        self.config["deadlines_seconds"]["default"] = 2
        self.config["deadlines_seconds"]["report"] = 2
        self.config["stdout_caps_bytes"]["aggregate"] = 4096
        digest = hashlib.sha256(self.source.read_bytes()).hexdigest()
        self.config["tested_binary_sha256"] = digest
        self.patch_hash = patch.object(RUN, "BINARY_SHA256", digest)
        self.patch_size = patch.object(RUN, "BINARY_BYTES", self.source.stat().st_size)
        self.patch_hash.start()
        self.patch_size.start()
        self.config_path = self.root/"config.json"
        self.config_path.write_text(json.dumps(self.config))
        self.counter = self.root/"counter"

    def tearDown(self):
        self.patch_hash.stop()
        self.patch_size.stop()
        self.temp.cleanup()

    def runner(self):
        return RUN.Runner(RUN.initialize(self.config_path))

    def test_exact_copy_binding_no_clobber_and_successful_resume(self):
        before = self.source.read_bytes()
        runner = self.runner()
        try:
            result = runner.require("operation", ["report", "ok", self.counter])
            self.assertTrue(result["value"]["fixture"])
            self.assertEqual(runner.call("operation", ["report", "ok", self.counter]), result)
            self.assertEqual(self.counter.read_text(), "1")
            with self.assertRaises(FileExistsError):
                RUN.initialize(self.config_path)
            self.assertEqual(self.source.read_bytes(), before)
        finally:
            runner.close()
        runner = RUN.Runner(self.root/"run")
        try:
            self.assertTrue(runner.require("operation", ["report", "ok", self.counter])["ok"])
            self.assertEqual(self.counter.read_text(), "1")
        finally:
            runner.close()

    def test_stdout_is_bounded_failure_is_retained_and_never_blindly_retried(self):
        runner = self.runner()
        try:
            result = runner.call("flood", ["report", "flood", self.counter])
            self.assertFalse(result["ok"])
            self.assertIn("output_limit", result["record"]["failure"])
            self.assertLessEqual(result["record"]["stdout"]["bytes"], 4096)
            self.assertFalse(runner.call("flood", ["report", "flood", self.counter])["ok"])
            self.assertEqual(self.counter.read_text(), "1")
        finally:
            runner.close()

    def test_deadline_kills_owned_process_and_journals_failure(self):
        runner = self.runner()
        try:
            result = runner.call("deadline", ["report", "sleep", self.counter])
            self.assertEqual(result["record"]["failure"], "deadline")
            self.assertLess(result["record"]["exit_code"], 0)
            self.assertFalse(result["ok"])
        finally:
            runner.close()

    def test_interrupted_journal_and_tampered_output_do_not_become_success(self):
        runner = self.runner()
        try:
            step = runner.root/"steps"/(RUN.sha(RUN.encoded("unfinished"))+".json")
            RUN.durable_json(step, {"record": "commands/absent/result.json", "sequence": 999})
            with self.assertRaises(RUN.InterruptedOperation):
                runner.call("unfinished", ["report", "ok", self.counter])
            self.assertFalse(self.counter.exists())
            result = runner.call("real", ["report", "ok", self.counter])
            (runner.root/result["record"]["stdout"]["path"]).write_text('{}')
            with self.assertRaisesRegex(ValueError, "digest changed"):
                runner.call("real", ["report", "ok", self.counter])
        finally:
            runner.close()

    def test_prohibited_assertions_commands_and_overlap_fail_before_original_io(self):
        invalid = dict(self.config, closed_application_evidence="invented")
        with self.assertRaises(ValueError):
            RUN.validate_config(invalid)
        overlap = dict(self.config, exclusive_output=self.config["catalog_root"])
        self.config_path.write_text(json.dumps(overlap))
        with self.assertRaisesRegex(ValueError, "overlaps"):
            RUN.initialize(self.config_path)
        self.assertFalse(Path(overlap["catalog_root"]).exists())
        self.config_path.write_text(json.dumps(self.config))
        runner = self.runner()
        try:
            with self.assertRaises(ValueError):
                runner.call("prohibited", ["choose", "anything"])
        finally:
            runner.close()

    def test_pages_keep_revision_scopes_distinct_and_reject_wrong_revision(self):
        runner = RUN.Runner.__new__(RUN.Runner)
        runner.config = copy.deepcopy(RUN.DEFAULTS)
        keys = []
        def require(key, arguments):
            keys.append(key)
            revision = arguments[2]
            value = [] if arguments[4] else [{"sequence": 1, "source_id": "fixture", "revision_id": revision, "table": "t"}]
            return {"value": value, "record": {"sequence": len(keys)}}
        runner.require = require
        self.assertEqual(runner.pages("plan", "r1", "rows", "scope")["rows"], 1)
        self.assertEqual(runner.pages("plan", "r2", "rows", "scope")["rows"], 1)
        self.assertEqual(len({RUN.sha(RUN.encoded(key)) for key in keys}), 4)
        runner.require = lambda key, args: {"value": [{"sequence": 1, "source_id": "fixture", "revision_id": "wrong", "table": "t"}], "record": {"sequence": 1}}
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            runner.pages("plan", "r1", "rows", "scope")

    def test_changed_full_inventory_and_failed_full_capture_block_before_direct_io(self):
        runner = RUN.Runner.__new__(RUN.Runner)
        runner.root = self.root
        runner.config = self.config
        calls = []
        runner.require = lambda key, arguments: calls.append(arguments)
        clean = {"added": [], "removed": [], "changed": [], "before_complete": True, "after_complete": True}
        valid = {"key": "a", "full_requested": True, "full": {"ok": True, "capture_path": "full", "capture": {"state": "captured", "raw_byte_retention": "complete", "sqlite_consistency": "consistent_default_sqlite"}}, "inspection": {"revision": "r", "capture_path": "full"}}
        for change in ["changed", "incomplete", "failed", "fallback"]:
            review = {"outcomes": [copy.deepcopy(valid)], "inventory_delta": copy.deepcopy(clean)}
            if change == "changed":
                review["inventory_delta"]["changed"] = ["source"]
            elif change == "incomplete":
                review["inventory_delta"]["after_complete"] = False
            elif change == "failed":
                review["outcomes"][0]["full"]["ok"] = False
            else:
                review["outcomes"][0]["inspection"]["capture_path"] = "main-only"
            path = self.root/(change+".json")
            path.write_text(json.dumps(review))
            with self.assertRaisesRegex(ValueError, "not admitted"):
                runner.path_phase(path, False)
            self.assertEqual(calls, [])

    def test_fresh_inventory_change_is_retained_before_direct_io(self):
        runner = RUN.Runner.__new__(RUN.Runner)
        runner.root = self.root
        (self.root/"reports").mkdir()
        runner.config = self.config
        before = {"complete": True, "candidates": [{"path": {"encoding": "UnixBytes", "units": list(b"/fixture.lrcat")}, "bytes": 1}]}
        after = copy.deepcopy(before)
        after["candidates"][0]["bytes"] = 2
        runner.command_document = lambda sequence, command: before
        calls = []
        def require(key, arguments):
            calls.append(arguments[0])
            return {"value": after, "record": {"sequence": 2}}
        runner.require = require
        review = {"inventory_delta": RUN.inventory_delta(before, before), "ending_inventory_command": 1, "outcomes": []}
        path = self.root/"review.json"
        path.write_text(json.dumps(review))
        with self.assertRaisesRegex(ValueError, "fresh inventory differs"):
            runner.path_phase(path, False)
        self.assertEqual(calls, ["discover"])
        records = list((self.root/"reports").glob("paths-admission-*.json"))
        self.assertEqual(len(records), 1)
        self.assertFalse(json.loads(records[0].read_text())["admitted"])

    def test_end_inventory_change_remains_visible_and_blocks_packet_admission(self):
        runner = RUN.Runner.__new__(RUN.Runner)
        runner.root = self.root
        (self.root/"reports").mkdir()
        runner.config = self.config
        before = {"complete": True, "candidates": [{"path": {"encoding": "UnixBytes", "units": list(b"/fixture.lrcat")}, "bytes": 1}]}
        after = copy.deepcopy(before)
        after["candidates"][0]["bytes"] = 2
        runner.command_document = lambda sequence, command: before
        calls = []
        discoveries = []
        def require(key, arguments):
            calls.append(arguments[0])
            if arguments[0] == "discover":
                discoveries.append(True)
                value = before if len(discoveries) == 1 else after
            elif arguments[0] == "check-paths":
                value = {"checked": 0}
            else:
                value = {}
            return {"value": value, "record": {"sequence": len(calls)+1}}
        runner.require = require
        runner.pages = lambda *args: {"rows": 0, "counts": {}}
        member = {"key": "a", "full_requested": True, "full": {"ok": True, "capture_path": "full", "capture": {"state": "captured", "raw_byte_retention": "complete", "sqlite_consistency": "consistent_default_sqlite"}}, "inspection": {"revision": "r", "capture_path": "full"}}
        review = {"plan": "fixture-plan", "inventory_delta": RUN.inventory_delta(before, before), "ending_inventory_command": 1, "outcomes": [member]}
        path = self.root/"review.json"
        path.write_text(json.dumps(review))
        output = runner.path_phase(path, False)
        result = json.loads(output.read_text())
        self.assertTrue(result["inventory_delta"]["changed"])
        self.assertIn("check-paths", calls)
        count = len(calls)
        with self.assertRaisesRegex(ValueError, "assessment is incomplete/changed"):
            runner.path_phase(path, True)
        self.assertEqual(len(calls), count)

    def test_coordinator_pause_is_at_boundary_and_completed_prefix_still_replays(self):
        runner = self.runner()
        try:
            pause = runner.root/"pause-request"
            completed = runner.call("first", ["report", "pause", self.counter, pause])
            self.assertTrue(completed["ok"], "request arriving inside current child must not interrupt it")
            before = RUN.read_json(runner.root/"journal.json")
            self.assertEqual(runner.call("first", ["report", "pause", self.counter, pause]), completed)
            with self.assertRaises(RUN.PauseRequested):
                runner.call("second", ["report", "ok", self.counter])
            self.assertEqual(RUN.read_json(runner.root/"journal.json"), before)
            self.assertEqual(self.counter.read_text(), "1")
            self.assertEqual(len(list((runner.root/"commands").iterdir())), 1)
            pause.unlink()  # Coordinator-owned fixture control, never automatic runner cleanup.
            self.assertTrue(runner.call("second", ["report", "ok", self.counter])["ok"])
            self.assertEqual(self.counter.read_text(), "2")
        finally:
            runner.close()

    def seed_fixture(self):
        """Synthetic native boundary model; not evidence of SQLite capture safety."""
        runner = self.runner()
        old = self.root/"predecessor"
        (old/"plans/main").mkdir(parents=True)
        (old/"plans/main/inspection.sqlite3").write_bytes(b"synthetic stopped plan and retained rows")
        (old/"runner.lock").touch()
        (old/"pause-request").write_text("held")
        RUN.durable_json(old/"journal.json", {"next_command": 31})
        binding = {"source": "old-core", "binary_sha256": "old-binary"}
        candidate = {"path": {"encoding": "UnixBytes", "units": list(os.fsencode(self.root/"catalogs/2022.lrcat"))}, "bytes": 10}
        inventory = {"complete": True, "candidates": [candidate]}
        capture = {"state": "captured", "revision_id": "raw-revision", "sqlite_consistency": "consistent_default_sqlite",
                   "raw_byte_retention": "auxiliary_omitted_for_discovery", "artifacts": [], "application_consistency": "unverified"}
        (old/"captures/first").mkdir(parents=True)
        def document(name, value):
            path = old/name
            path.parent.mkdir(parents=True, exist_ok=True)
            RUN.durable_json(path, value)
            return {"path": name, "sha256": RUN.file_sha(path)}
        request = {"protocol": 1, "predecessor": str(old), "revision": "raw-revision",
                   "candidate_key": RUN.candidate_key(candidate), "capture_path": "captures/first",
                   "expected_rows": 1, "expected_table_counts": {"Opaque": 1, "Empty": 0}}
        request["binding"] = document("binding.json", binding)
        request["paused_phase"] = document("phase.json", {"status": "paused", "binding": binding})
        request["failed_result"] = document("commands/000000030/result.json", {"sequence": 30, "exit_code": -15, "source_binding": binding,
            "requested_arguments": ["resume", str(old/"plans/main"), "raw-revision", "--max-rows", "10000"]})
        request["stop_decision"] = document("stop.json", {"reason": "synthetic approved stop"})
        request["table_state_evidence"] = document("tables.json", {"rows": 1})
        request["inventory_result"] = document("commands/000000001/result.json", {"sequence": 1, "exit_code": 0,
            "failure": None, "log_errors": [], "source_binding": binding, "requested_arguments": ["discover", runner.config["catalog_root"]],
            "stdout": document("inventory.json", inventory)})
        request["capture_result"] = document("commands/000000004/result.json", {"sequence": 4, "exit_code": 0,
            "failure": None, "log_errors": [], "source_binding": binding,
            "requested_arguments": ["capture", str(RUN.native_path(candidate["path"])), "@CAPTURE@", "--main-only"],
            "capture_path": str(old/"captures/first"), "stdout": document("capture.json", capture)})
        runner.config["seed_request"] = request
        runner.fixture_calls = []
        runner.fixture_after_rows = [{"sequence": 7, "source_id": "stable-source-id", "revision_id": "raw-revision",
                                      "table": "Opaque", "cells": [{"type": "Blob", "value": "00ff"}]}]
        before_rows = copy.deepcopy(runner.fixture_after_rows)
        tables = [{"name": name, "retained": count, "expected": count, "state": "complete"}
                  for name, count in request["expected_table_counts"].items()]
        runner.fixture_report = {"revision_id": "raw-revision", "lineage_id": "stable-lineage", "stage": "pending",
                                 "capture": capture, "tables": tables, "counts": {"opaque": 1}}
        reports = {}
        cache = {}
        def require(key, args):
            serialized = RUN.sha(RUN.encoded(key))
            if serialized in cache:
                return cache[serialized]
            runner.fixture_calls.append([str(v) for v in args])
            sequence = len(runner.fixture_calls)
            record = {"sequence": sequence}
            if args[0] == "capture":
                directory = runner.root/"captures/private-plan"
                directory.mkdir()
                logical = directory/"logical.sqlite3"
                logical.write_bytes((old/"plans/main/inspection.sqlite3").read_bytes())
                record["capture_path"] = str(directory)
                value = {"state": "captured", "raw_byte_retention": "complete", "sqlite_consistency": "consistent_default_sqlite",
                         "logical_revision": RUN.native_revision(logical.stat())}
            elif args[0] == "create":
                Path(args[1]).mkdir()
                value = {}
            elif args[0] == "add":
                value = {"revision": "raw-revision"}
            elif args[0] == "families":
                value = {"families": [{"selected": None, "members": [{"revision_id": "raw-revision"}]}]}
            elif args[0] == "report":
                value = copy.deepcopy(runner.fixture_report)
                reports[sequence] = value
            elif args[0] == "resume":
                runner.fixture_report["stage"] = "rows_reconciled_paths_pending"
                value = {"stage": "rows_reconciled_paths_pending", "retained_this_call": 0}
            elif args[0] in RUN.PAGES:
                value = []
                if args[0] == "rows" and args[4] == 0:
                    value = copy.deepcopy(before_rows if runner.fixture_report["stage"] == "pending" else runner.fixture_after_rows)
            else:
                raise AssertionError(args)
            cache[serialized] = {"value": value, "record": record}
            return cache[serialized]
        runner.require = require
        runner.command_document = lambda seq, command: reports[seq]
        return runner, old

    def test_seed_adoption_preserves_rows_ids_raw_evidence_and_replays_without_forged_commands(self):
        runner, old = self.seed_fixture()
        before = {str(p.relative_to(old)): p.read_bytes() for p in old.rglob("*") if p.is_file()}
        try:
            receipt = RUN.read_json(runner.seed_phase())
            self.assertEqual(receipt["inspection"]["pages"]["rows"]["content_sha256"], receipt["before_rows"]["content_sha256"])
            self.assertEqual(receipt["inspection"]["pages"]["rows"]["identity_sha256"], receipt["before_rows"]["identity_sha256"])
            self.assertEqual(receipt["failed_command"]["exit_code"], -15)
            self.assertFalse(receipt["automatic_selection"])
            self.assertFalse(receipt["migration_executed"])
            self.assertEqual(receipt["lineage_id"], "stable-lineage")
            self.assertEqual(receipt["capture_path"], str(old/"captures/first"))
            self.assertEqual([c for c in runner.fixture_calls if c[0] == "capture"],
                             [["capture", str(old/"plans/main/inspection.sqlite3"), "@CAPTURE@"]])
            self.assertNotIn(["create", str(runner.root/"plans/main")], runner.fixture_calls)
            count = len(runner.fixture_calls)
            self.assertEqual(RUN.read_json(runner.seed_phase()), receipt)
            self.assertEqual(len(runner.fixture_calls), count)
            self.assertEqual(before, {str(p.relative_to(old)): p.read_bytes() for p in old.rglob("*") if p.is_file()})
        finally:
            runner.close()

    def test_seed_rejects_changed_retained_ids_counts_inventory_and_evidence(self):
        runner, old = self.seed_fixture()
        try:
            runner.fixture_after_rows[0]["source_id"] = "corrupted-source-id"
            with self.assertRaisesRegex(ValueError, "changed retained rows or source IDs"):
                runner.seed_phase()
            self.assertFalse((runner.root/"reports/seed-adoption.json").exists())
            bad = copy.deepcopy(runner.config["seed_request"])
            bad["expected_rows"] = 2
            with self.assertRaisesRegex(ValueError, "table counts"):
                RUN.seed_table_counts(runner.fixture_report, bad)
            (old/"inventory.json").write_text('{}')
            with self.assertRaisesRegex(ValueError, "digest/size mismatch"):
                runner.seed_phase()
        finally:
            runner.close()

    def test_seed_rejects_unpaused_predecessor_and_interrupted_copy(self):
        runner, old = self.seed_fixture()
        try:
            (old/"pause-request").unlink()
            with self.assertRaisesRegex(ValueError, "not held paused"):
                runner.seed_phase()
            self.assertFalse(runner.fixture_calls)
            (old/"pause-request").touch()
            RUN.durable_json(runner.root/"reports/seed-copy-started.json", {"interrupted": True})
            with self.assertRaisesRegex(RUN.InterruptedOperation, "unpublished seed copy"):
                runner.seed_phase()
            self.assertFalse((runner.root/"plans/main").exists())
            self.assertFalse(any(c[0] == "resume" for c in runner.fixture_calls))
        finally:
            runner.close()

    def test_seed_copy_rejects_changed_revision_and_never_overwrites_destination(self):
        source = self.root/"logical.sqlite3"
        source.write_bytes(b"retained bytes")
        expected = RUN.native_revision(source.stat())
        destination = self.root/"copied.sqlite3"
        receipt = RUN.copy_seed_snapshot(source, destination, expected)
        self.assertEqual(receipt["sha256"], RUN.file_sha(source))
        with self.assertRaises(FileExistsError):
            RUN.copy_seed_snapshot(source, destination, expected)
        source.write_bytes(b"replacement")
        with self.assertRaisesRegex(ValueError, "revision mismatch"):
            RUN.copy_seed_snapshot(source, self.root/"another.sqlite3", expected)
        self.assertFalse((self.root/"another.sqlite3").exists())

    def test_seed_main_admits_inventory_and_uses_explicit_adoption_without_source_recapture(self):
        runner, _ = self.seed_fixture()
        try:
            seed = RUN.read_json(runner.seed_phase())
            calls = []
            def require(key, arguments):
                calls.append(arguments)
                if arguments[0] == "discover":
                    value = seed["original_inventory"]
                elif arguments[0] == "register-inventory":
                    value = {}
                elif arguments[0] == "families":
                    value = {"families": [{"id": "family", "evidence_digest": "evidence", "suggested": None,
                        "selected": None, "members": [{"revision_id": "raw-revision"}], "issues": ["review required"]}]}
                else:
                    raise AssertionError("unexpected child (including create/capture/resume): " + repr(arguments))
                return {"value": value, "record": {"sequence": 100+len(calls), "stdout": {"path": "synthetic-inventory"}}}
            runner.require = require
            runner.call = lambda *args: self.fail("seed must not recapture the original")
            review = RUN.read_json(runner.main_phase())
            self.assertEqual(review["candidate_count"], 1)
            self.assertEqual(review["outcomes"][0]["inspection"], seed["inspection"])
            self.assertNotIn("capture_command", review["outcomes"][0], "no invented new-run capture command")
            self.assertIn("seed_adoption_sha256", review["outcomes"][0])
            self.assertFalse(review["automatic_selection"])
            self.assertEqual([c[0] for c in calls], ["discover", "register-inventory", "discover", "families"])
        finally:
            runner.close()

    def test_seed_main_rejects_inventory_delta_before_original_capture_or_plan_mutation(self):
        runner, _ = self.seed_fixture()
        try:
            seed = RUN.read_json(runner.seed_phase())
            changed = copy.deepcopy(seed["original_inventory"])
            changed["candidates"][0]["bytes"] += 1
            calls = []
            def require(key, arguments):
                calls.append(arguments)
                self.assertEqual(arguments[0], "discover")
                return {"value": changed, "record": {"sequence": 100}}
            runner.require = require
            with self.assertRaisesRegex(ValueError, "inventory changed"):
                runner.main_phase()
            self.assertEqual(len(calls), 1)
            admission = RUN.read_json(runner.root/"reports/seed-inventory-admission.json")
            self.assertFalse(admission["admitted"])
            self.assertEqual(admission["delta"]["changed"], [seed["candidate_key"]])
        finally:
            runner.close()

    def test_unbound_corrected_binary_fails_before_output_creation(self):
        with patch.object(RUN, "BINARY_SHA256", None):
            with self.assertRaisesRegex(ValueError, "no tested binary binding"):
                RUN.initialize(self.config_path)
        self.assertFalse((self.root/"run").exists())


if __name__ == "__main__":
    unittest.main()
