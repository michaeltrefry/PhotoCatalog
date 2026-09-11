"""Tiny real SQLite schema2 clone plus two-origin command provenance fixtures."""
from contextlib import closing
import importlib.util
import json
from pathlib import Path
import shutil
import sqlite3
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("legacy_generation_fixture", Path(__file__).with_name("test_lightroom_generation.py"))
LEGACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LEGACY)
RUN, GEN = LEGACY.RUN, LEGACY.GEN


class RepairGenerationTests(unittest.TestCase):
    def setUp(self):
        self.f = LEGACY.GenerationTests(); self.f.setUp()
        f = self.f
        self.ancestor = f.old
        ancestor_binding = f.binding
        ancestor_prefix = {RUN.sha(RUN.encoded(RUN.read_json(self.ancestor/f"commands/{n:09d}/result.json")["key"])):
                           f.reference(f"commands/{n:09d}/result.json") for n in range(1, f.next)}
        done = RUN.read_json(self.ancestor/"reports/completed.json")
        f.old = f.root/"failed"; f.old.mkdir()
        for name in ["commands", "steps", "reports", "plans", "captures"]: (f.old/name).mkdir()
        (f.old/"runner.lock").touch()
        shutil.copytree(self.ancestor/"plans/main", f.old/"plans/main")
        with closing(sqlite3.connect(f.old/"plans/main/inspection.sqlite3")) as db:
            db.executescript(f.indexes+"PRAGMA user_version=2;")
        f.binding = {"source": "v4 synthetic predecessor", "binary_sha256": "fixture-only"}
        RUN.durable_json(f.old/"binding.json", f.binding)
        RUN.durable_json(f.old/"reports/generation-adoption.json", {
            "protocol":1,"source_binding":f.binding,"after_logical_state_equal":True,
            "predecessor":str(self.ancestor),"predecessor_binding":ancestor_binding,
            "predecessor_state":GEN.artifact_state(self.ancestor/"plans/main"),"prefix":ancestor_prefix})
        f.next = 1
        inventory = f.command(["fresh", "discover"], ["discover", f.config["catalog_root"]], f.inventory)
        f.command(f.active_logical_key, ["rows",str(f.old/"plans/main"),"active","--after","0","--limit","1"], [f.values["active"][0]])
        RUN.durable_json(f.old/"journal.json", {"next_command":f.next})
        done["inspection"]["report"] = f"adopted/{done['inspection']['report']:09d}"
        done["inspection"]["pages"]["rows"]["pages"] = [f"adopted/{n:09d}" for n in done["inspection"]["pages"]["rows"]["pages"]]
        RUN.durable_json(f.old/"reports/completed.json", done)
        self.control = f.root/"control"
        self.attempt = self.control/"attempts/00000000-0000-0000-0000-000000000001"
        self.attempt.mkdir(parents=True)
        failed = {"status":"failed_or_unknown","exit_code":-2,"failure":"RuntimeError('sampled observed RSS stop threshold exceeded')",
                  "started_unix":10,"ownership_status":"unknown_requires_review",
                  "cleanup":{"root_reaped":True,"remaining_observed":[],"errors":[],"ownership_uncertainties":[]}}
        RUN.durable_json(self.attempt/"result.json", failed)
        RUN.durable_json(self.control/"current.json", {"attempt_id":self.attempt.name})
        RUN.durable_json(f.old/"reports/failed-phase.json", {"phase":"main","status":"failed_or_interrupted","started_unix":11,"binding":f.binding})
        review = {"status":"PASS","failed_result_sha256":RUN.file_sha(self.attempt/"result.json"),
                  "phase_sha256":RUN.file_sha(f.old/"reports/failed-phase.json"),"next_command":f.next,
                  "known_processes_absent":True,"no_unresolved_command":True}
        RUN.durable_json(self.control/"ownership-review.json", review)
        prefix = {RUN.sha(RUN.encoded(RUN.read_json(f.old/f"commands/{n:09d}/result.json")["key"])):
                  f.reference(f"commands/{n:09d}/result.json") for n in range(1,f.next)}
        request = f.request
        request.update(protocol=2, predecessor=str(f.old), binding=f.reference("binding.json"),journal=f.reference("journal.json"),
                       expected_next_command=f.next,inventory_result=inventory,command_index_sha256=RUN.sha(RUN.encoded(prefix)),
                       completed_outcomes=[f.reference("reports/completed.json")],inherited_adoption=f.reference("reports/generation-adoption.json"),
                       expected_source_state=GEN.artifact_state(f.old/"plans/main"),failed_predecessor={
                           "result":self.absolute(self.attempt/"result.json"),"phase":self.absolute(f.old/"reports/failed-phase.json"),
                           "control":self.absolute(self.control/"current.json"),"ownership_review":self.absolute(self.control/"ownership-review.json")})
        request.pop("pause")

    def tearDown(self): self.f.tearDown()

    def absolute(self, path): return {"path":str(path),"sha256":RUN.file_sha(path)}

    def source_tree(self):
        return {str(p):RUN.file_sha(p) for root in [self.f.old,self.ancestor,self.control] for p in root.rglob('*') if p.is_file()}

    def test_schema2_clone_once_and_distinct_local_inherited_reference_spaces(self):
        before = self.source_tree()
        with patch.object(GEN, "typed_scan", wraps=GEN.typed_scan) as scan:
            path = GEN.adopt(self.f.runner)
        self.assertEqual(scan.call_count,1)
        receipt = RUN.read_json(path)
        self.assertEqual(self.source_tree(),before)
        self.assertEqual(self.f.calls,[])
        self.assertEqual(list((self.f.new/"commands").iterdir()),[])
        self.assertEqual(receipt["actual_native_commands"],0)
        self.assertEqual(receipt["failure_proof"]["failure_classification_retained"],"failed_or_unknown")
        self.assertEqual(RUN.file_sha(self.f.old/"plans/main/inspection.sqlite3"),RUN.file_sha(self.f.new/"plans/main/inspection.sqlite3"))
        replay = GEN.RepairReplay(self.f.runner)
        try:
            outcome = replay.outcome(RUN.candidate_key(self.f.candidates[0]), self.f.candidates[0])
            self.assertEqual(outcome["inspection"]["report"],"adopted/inherited/000000002")
            self.assertEqual(replay.document(outcome["inspection"]["report"],"report")["revision_id"],"done")
            result = replay.replay(self.f.active_logical_key,["rows",self.f.new/"plans/main","active","--after",0,"--limit",1])
            self.assertEqual(result["record"]["sequence"],"adopted/local/000000002")
            self.assertEqual(result["value"],self.f.values["active"][:1])
            self.assertIsNone(replay.replay([["main",self.f.active_key],"active","rows",1],["rows",self.f.new/"plans/main","active","--after",3,"--limit",1]))
            with self.assertRaisesRegex(ValueError,"unexpected inherited"):
                replay.document("adopted/local/000000002","report")
        finally: replay.close()

    def test_missing_ownership_review_never_copies_or_fabricates_pause(self):
        self.f.request["failed_predecessor"]["ownership_review"]["sha256"] = "0"*64
        with self.assertRaisesRegex(ValueError,"evidence changed"): GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/"plans/main").exists())
        self.assertFalse((self.f.old/"pause-request").exists())
        with self.assertRaises(RUN.InterruptedOperation): GEN.adopt(self.f.runner)

    def test_wrong_failed_phase_binding_or_control_is_rejected(self):
        path = self.f.old/"reports/failed-phase.json";value=RUN.read_json(path);value["binding"]={"source":"other"}
        RUN.durable_json(path,value,replace=True)
        self.f.request["failed_predecessor"]["phase"] = self.absolute(path)
        with self.assertRaisesRegex(ValueError,"reviewed reaped"): GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/"plans/main").exists())

    def test_inherited_hash_mutation_and_lineage_cycle_are_not_local_fallback(self):
        path=self.ancestor/"commands/000000002/result.json";value=RUN.read_json(path);value["source_binding"]={"changed":True}
        RUN.durable_json(path,value,replace=True)
        with self.assertRaises(ValueError): GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/"plans/main").exists())

    def test_inherited_cycle_is_rejected_before_copy(self):
        path=self.f.old/"reports/generation-adoption.json";value=RUN.read_json(path);value["predecessor"]=str(self.f.old)
        RUN.durable_json(path,value,replace=True);self.f.request["inherited_adoption"]=self.f.reference("reports/generation-adoption.json")
        with self.assertRaisesRegex(ValueError,"cycle"): GEN.adopt(self.f.runner)

    def test_failed_local_command_is_not_adoptable_even_with_matching_index(self):
        path=self.f.old/"commands/000000002/result.json";value=RUN.read_json(path);value["exit_code"]=-9
        RUN.durable_json(path,value,replace=True)
        with self.assertRaisesRegex(ValueError,"failed/unresolved"): GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/"plans/main").exists())

    def test_wrong_schema_copy_remains_failed_partial_and_cannot_retry(self):
        with closing(sqlite3.connect(self.f.old/"plans/main/inspection.sqlite3")) as db:db.execute("PRAGMA user_version=1")
        self.f.request["expected_source_state"]=GEN.artifact_state(self.f.old/"plans/main")
        with self.assertRaisesRegex(ValueError,"schema/count"): GEN.adopt(self.f.runner)
        self.assertTrue((self.f.new/"plans/main/inspection.sqlite3").exists())
        self.assertFalse((self.f.new/"reports/generation-adoption.json").exists())
        with self.assertRaises(RUN.InterruptedOperation): GEN.adopt(self.f.runner)

    def test_partial_copy_is_retained_and_no_ready_receipt_published(self):
        def partial(source,destination,*args):
            destination.write_bytes(b"owned partial")
            raise ValueError("copy fixture interruption")
        with patch.object(GEN,"copy_artifact",side_effect=partial):
            with self.assertRaisesRegex(ValueError,"interruption"):GEN.adopt(self.f.runner)
        self.assertEqual((self.f.new/"plans/main/inspection.sqlite3").read_bytes(),b"owned partial")
        self.assertFalse((self.f.new/"reports/generation-adoption.json").exists())
        with self.assertRaises(RUN.InterruptedOperation): GEN.adopt(self.f.runner)

    def test_retained_count_mismatch_cannot_publish_ready(self):
        self.f.request["expected_rows"]["active"] = 3
        with self.assertRaisesRegex(ValueError, "schema/count"):
            GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/"reports/generation-adoption.json").exists())

    def test_active_cursor_and_gap_are_not_trusted_from_request(self):
        self.f.request["active"]["after"] = 999
        with self.assertRaisesRegex(ValueError, "active cursor"):
            GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/"plans/main").exists())

    def test_replay_rejects_changed_origin_or_prefix_in_receipt(self):
        receipt_path = GEN.adopt(self.f.runner)
        original = RUN.read_json(receipt_path)
        for field in ["binding", "prefix"]:
            with self.subTest(field=field):
                value = json.loads(json.dumps(original))
                value["origins"]["inherited"][field] = {}
                RUN.durable_json(receipt_path, value, replace=True)
                with self.assertRaisesRegex(ValueError, "lineage/index"):
                    GEN.RepairReplay(self.f.runner)
        RUN.durable_json(receipt_path, original, replace=True)
        replay = GEN.RepairReplay(self.f.runner)
        try:
            for token in ["adopted/2", "adopted/-00000002", "adopted/00000000²", "adopted/other/000000002", True]:
                with self.subTest(token=token), self.assertRaises(ValueError):
                    replay.reference(token)
        finally:
            replay.close()

    def test_origin_identity_binding_change_after_adoption_rejected(self):
        GEN.adopt(self.f.runner)
        RUN.durable_json(self.ancestor/"binding.json", {"foreign":True}, replace=True)
        with self.assertRaisesRegex(ValueError, "origin changed"):
            GEN.RepairReplay(self.f.runner)


    def test_nonempty_companion_requires_explicit_recovery_instead_of_immutable_read(self):
        (self.f.old/"plans/main/inspection.sqlite3-wal").write_bytes(b"not ignored")
        with self.assertRaisesRegex(ValueError,"nonempty WAL"):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/"plans/main").exists())


if __name__ == "__main__": unittest.main()
