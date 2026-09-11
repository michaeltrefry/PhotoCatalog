"""Disposable schema/copy/replay tests; no originals and no performance claims."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("generation_runner", Path(__file__).parents[1]/"scripts/run_lightroom_inspection.py")
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)
GEN = RUN.generation_tools()


def native(path):
    return {"encoding": "UnixBytes", "units": list(os.fsencode(path))}


@unittest.skipUnless(os.name == "posix", "private generation coordinator requires POSIX ownership locks")
class GenerationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.old = self.root/"old"
        self.new = self.root/"new"
        for root in [self.old, self.new]:
            for relative in ["commands", "steps", "reports", "plans", "captures"]:
                (root/relative).mkdir(parents=True)
        (self.old/"plans/main").mkdir()
        (self.old/"runner.lock").touch()
        self.binding = {"source": "synthetic predecessor", "binary_sha256": "fixture-only"}
        RUN.durable_json(self.old/"binding.json", self.binding)
        RUN.durable_json(self.old/"pause-request", {"owner": "fixture coordinator"})
        self.config = copy.deepcopy(RUN.DEFAULTS)
        self.config.update(catalog_root=str(self.root/"catalogs"), original_root=str(self.root/"originals"), minimum_free_bytes=1, page_limit=1)
        source = (Path(__file__).parents[1]/"src/lightroom/plan.rs").read_text()
        schema = re.search(r'transaction.execute_batch\("(PRAGMA application_id=.*?)"\)\?;', source, re.S).group(1)
        self.indexes = re.search(r'const PAGING_INDEXES: &str = "(.*?)";', source, re.S).group(1)
        db = sqlite3.connect(self.old/"plans/main/inspection.sqlite3")
        db.executescript(schema+"PRAGMA user_version=1;")
        self.values = {}
        self.candidates = []
        for i, revision in enumerate(["done", "active"]):
            capture = self.old/"captures"/revision
            capture.mkdir()
            candidate = {"path": native(Path(self.config["catalog_root"])/(revision+".lrcat")), "bytes": 100, "modified_ns": 1}
            manifest = {"fixture": revision, "request": {"source": candidate["path"]}}
            RUN.durable_json(capture/"manifest.json", manifest)
            self.candidates.append(candidate)
            db.execute("INSERT INTO captures(revision,lineage,path,manifest,stage) VALUES(?,?,?,?, 'rows_reconciled_paths_pending')", [revision,"lineage-"+revision,json.dumps(native(capture)),json.dumps(manifest)])
            db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?, 'source','[\"value\"]','[]','{}','retained_only',2,2,'complete')", [revision])
            rows = []
            for j in range(2):
                sequence = 2*i+j+1
                source_id = f"{revision}-{j}"
                cells = [{"type": "Text", "value": "00ff"}, {"type": "RealBits", "value": 9223372036854775808}]
                db.execute("INSERT INTO rows VALUES(?,?,?,'source',?,?)",[sequence,revision,source_id,json.dumps([j]),json.dumps(cells)])
                rows.append({"sequence":sequence,"source_id":source_id,"revision_id":revision,"table":"source","source_key":[j],"columns":["value"],"cells":cells,"semantics":"retained_only"})
            self.values[revision] = rows
        db.commit()
        db.close()
        self.next = 1
        self.inventory = {"complete":True,"candidates":self.candidates}
        inventory_ref = self.command(["main","discover"],["discover",self.config["catalog_root"]],self.inventory)
        done_key = RUN.candidate_key(self.candidates[0])
        report = {"revision_id":"done","lineage_id":"lineage-done","stage":"rows_reconciled_paths_pending","tables":[{"name":"source","retained":2}]}
        report_ref = self.command([["main",done_key],"report"],["report",str(self.old/"plans/main"),"done"],report)
        pages = []
        after = 0
        for number, value in enumerate(self.values["done"]):
            reference = self.command([["main",done_key],"done","rows",number],["rows",str(self.old/"plans/main"),"done","--after",str(after),"--limit","1"],[value])
            pages.append(self.next-1)
            after = value["sequence"]
        references = [{"path":f"commands/{sequence:09d}/result.json","sha256":RUN.file_sha(self.old/f"commands/{sequence:09d}/result.json")} for sequence in pages]
        summary = GEN.verify_pages(self.old,self.binding,references,"done",1)
        summary["pages"] = pages
        outcome = {"candidate":self.candidates[0],"key":done_key,"status":"inspected_with_reported_limits","capture_path":str(self.old/"captures/done"),
                   "inspection":{"revision":"done","capture_path":str(self.old/"captures/done"),"report":json.loads((self.old/report_ref["path"]).read_text())["sequence"],"stage":report["stage"],"pages":{"rows":summary}}}
        RUN.durable_json(self.old/"reports/completed.json",outcome)
        self.active_key = RUN.candidate_key(self.candidates[1])
        self.active_logical_key = [["main",self.active_key],"active","rows",0]
        self.command(self.active_logical_key,["rows",str(self.old/"plans/main"),"active","--after","0","--limit","1"],[self.values["active"][0]])
        RUN.durable_json(self.old/"journal.json",{"next_command":self.next})
        prefix = {}
        for sequence in range(1,self.next):
            relative = f"commands/{sequence:09d}/result.json"
            record = RUN.read_json(self.old/relative)
            prefix[RUN.sha(RUN.encoded(record["key"]))] = self.reference(relative)
        self.request = {"protocol":1,"predecessor":str(self.old),"binding":self.reference("binding.json"),"journal":self.reference("journal.json"),"pause":self.reference("pause-request"),
                        "inventory_result":inventory_ref,"expected_next_command":self.next,"command_index_sha256":RUN.sha(RUN.encoded(prefix)),
                        "completed_outcomes":[self.reference("reports/completed.json")],"active":{"candidate_key":self.active_key,"revision":"active","page_next":1,"after":3},
                        "expected_source_state":GEN.artifact_state(self.old/"plans/main"),"expected_table_counts":{"done":{"source":2},"active":{"source":2}},"expected_rows":{"done":2,"active":2},
                        "limits":{"plan_bytes":10*RUN.MIB,"copy_seconds":20,"scan_seconds":20,"scan_rows":1000,"scan_row_bytes":RUN.MIB,"commands":100,"captures":10}}
        self.config["generation_request"] = self.request
        outer = self
        class Stub:
            root=outer.new
            config=outer.config
            binding={"source":"synthetic corrected core"}
            def summary(self,name,value):
                path=self.root/"reports"/(name+".json")
                RUN.durable_json(path,value)
                return path
            def space(self,*args):return {}
            def require(self,key,args):
                if key != ["generation","upgrade"]:raise AssertionError("unexpected child")
                outer.calls.append([str(v) for v in args])
                db=sqlite3.connect(self.root/"plans/main/inspection.sqlite3")
                db.executescript("BEGIN;"+outer.indexes+"PRAGMA user_version=2;COMMIT;")
                if outer.mutate_upgrade:db.execute("UPDATE rows SET source_id='wrong' WHERE sequence=4")
                db.commit();db.close()
                return {"record":{"sequence":1},"ok":True,"value":[]}
        self.runner=Stub()
        self.calls=[]
        self.mutate_upgrade=False

    def tearDown(self):self.temp.cleanup()

    def reference(self,relative):
        return {"path":relative,"sha256":RUN.file_sha(self.old/relative)}

    def command(self,key,arguments,value):
        sequence=self.next;self.next+=1
        directory=self.old/"commands"/f"{sequence:09d}"
        directory.mkdir()
        RUN.durable_json(directory/"stdout",value)
        record={"sequence":sequence,"key":key,"requested_arguments":arguments,"source_binding":self.binding,"stdout_cap":8*RUN.MIB,
                "stdout":{"path":str((directory/"stdout").relative_to(self.old)),"sha256":RUN.file_sha(directory/"stdout"),"bytes":(directory/"stdout").stat().st_size},
                "exit_code":0,"failure":None,"log_errors":[]}
        RUN.durable_json(directory/"result.json",record)
        relative=str((directory/"result.json").relative_to(self.old))
        RUN.durable_json(self.old/"steps"/(RUN.sha(RUN.encoded(key))+".json"),{"record":relative,"sequence":sequence})
        return self.reference(relative)

    def tree(self):
        return {str(p.relative_to(self.old)):RUN.file_sha(p) for p in self.old.rglob('*') if p.is_file()}

    def test_adoption_preserves_source_typed_state_and_references_without_fake_commands(self):
        before=self.tree()
        path=GEN.adopt(self.runner)
        receipt=RUN.read_json(path)
        self.assertEqual(self.tree(),before)
        self.assertEqual(len(self.calls),1)
        self.assertTrue(receipt["after_logical_state_equal"])
        self.assertEqual(receipt["active_verified_prefix"]["last_sequence"],3)
        replay=GEN.Replay(self.runner)
        try:
            result=replay.replay(self.active_logical_key,["rows",self.new/"plans/main","active","--after",0,"--limit",1])
            self.assertEqual(result["record"]["kind"],"adopted_reference")
            self.assertTrue(result["record"]["sequence"].startswith("adopted/"))
            self.assertEqual(result["value"],self.values["active"][:1])
            self.assertIsNone(replay.replay([["main",self.active_key],"active","rows",1],["rows",self.new/"plans/main","active","--after",3,"--limit",1]))
            with self.assertRaises(ValueError):replay.replay(self.active_logical_key,["rows",self.new/"plans/main","active","--after",2,"--limit",1])
        finally:replay.close()
        self.assertEqual(list((self.new/"commands").iterdir()),[])

    def test_changed_upgrade_retains_failed_evidence_and_refuses_retry(self):
        self.mutate_upgrade=True
        with self.assertRaisesRegex(ValueError,"logical data"):GEN.adopt(self.runner)
        self.assertTrue((self.new/"reports/generation-after.json").exists())
        self.assertFalse((self.new/"reports/generation-adoption.json").exists())
        with self.assertRaises(RUN.InterruptedOperation):GEN.adopt(self.runner)
        self.assertEqual(len(self.calls),1)

    def test_nonempty_wal_wrong_cursor_or_checkpoint_is_rejected_before_copy(self):
        original=copy.deepcopy(self.request)
        for case in ["wal","cursor","index","state"]:
            with self.subTest(case=case):
                self.request.clear();self.request.update(copy.deepcopy(original))
                wal=self.old/"plans/main/inspection.sqlite3-wal"
                if case=="wal":wal.write_bytes(b"preserved not ignored")
                elif case=="cursor":self.request["active"]["after"]=4
                elif case=="index":self.request["command_index_sha256"]="0"*64
                else:self.request["expected_source_state"]["inspection.sqlite3"][2]+=1
                with self.assertRaises(ValueError):GEN.adopt(self.runner)
                self.assertFalse((self.new/"plans/main").exists())
                if wal.exists():wal.unlink()
        self.assertEqual(self.calls,[])

    def test_completed_counts_and_page_bytes_must_match_retained_state(self):
        self.request["expected_rows"]["done"]=999
        with self.assertRaisesRegex(ValueError,"counts/revisions"):GEN.adopt(self.runner)
        self.assertFalse((self.new/"reports/generation-adoption.json").exists())
        self.assertEqual(self.calls,[])

    def test_row_limit_and_text_blob_storage_classes_remain_explicit(self):
        dbpath=self.old/"plans/main/inspection.sqlite3"
        db=sqlite3.connect(dbpath)
        db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,decoded,detail) VALUES('active','p','catalog','digest',CAST(x'00ff' AS TEXT),x'00ff','detail')")
        db.commit();db.close()
        first=GEN.typed_scan(dbpath,self.request["limits"])
        db=sqlite3.connect(dbpath);db.execute("UPDATE packets SET raw=x'00ff'");db.commit();db.close()
        second=GEN.typed_scan(dbpath,self.request["limits"])
        self.assertNotEqual(first["tables"]["packets"]["sha256"],second["tables"]["packets"]["sha256"])
        tiny=dict(self.request["limits"],scan_rows=1)
        with self.assertRaisesRegex(ValueError,"row limit"):GEN.typed_scan(dbpath,tiny)

    def test_pause_prevents_copy_and_new_child(self):
        (self.new/"pause-request").write_text("owned pause")
        with self.assertRaises(RUN.PauseRequested):GEN.adopt(self.runner)
        self.assertFalse((self.new/"plans/main").exists())
        self.assertEqual(self.calls,[])

    def test_changed_saved_page_is_rejected_before_copy(self):
        last = self.old/f"commands/{self.next-1:09d}/stdout"
        last.write_text("[]\n")
        with self.assertRaisesRegex(ValueError,"digest/size mismatch"):
            GEN.adopt(self.runner)
        self.assertFalse((self.new/"plans/main").exists())
        self.assertEqual(self.calls,[])

    def test_runner_replays_adopted_prefix_under_pause_but_never_starts_new_child(self):
        GEN.adopt(self.runner)
        replay=GEN.Replay(self.runner)
        proxy=object.__new__(RUN.Runner)
        proxy.root=self.new
        proxy.generation=replay
        (self.new/"pause-request").write_text("owned pause")
        try:
            result=proxy.call(self.active_logical_key,["rows",self.new/"plans/main","active","--after",0,"--limit",1])
            self.assertEqual(result["record"]["kind"],"adopted_reference")
            with self.assertRaises(RUN.PauseRequested):
                proxy.call([["main",self.active_key],"active","rows",1],["rows",self.new/"plans/main","active","--after",3,"--limit",1])
            self.assertEqual(list((self.new/"commands").iterdir()),[])
            self.assertEqual(list((self.new/"steps").iterdir()),[])
        finally:replay.close()

    def test_generation_main_requires_new_whole_inventory_before_any_plan_command(self):
        from types import SimpleNamespace
        proxy=object.__new__(RUN.Runner)
        proxy.root=self.new
        proxy.config=self.config
        proxy.binding=self.runner.binding
        proxy.generation=SimpleNamespace(receipt={"original_inventory":self.inventory})
        RUN.durable_json(self.new/"journal.json",{"next_command":1})
        changed=copy.deepcopy(self.inventory)
        changed["candidates"][0]["bytes"]+=1
        calls=[]
        def require(key,args):
            calls.append((key,args))
            if args[0]!="discover":raise AssertionError("plan/source command crossed changed inventory")
            return {"value":changed,"record":{"sequence":1}}
        proxy.require=require
        with self.assertRaisesRegex(ValueError,"whole inventory changed"):
            proxy.main_phase()
        self.assertEqual(len(calls),1)
        self.assertEqual(calls[0][0][:2],["generation","admission"])
        self.assertEqual(calls[0][0][-1],"discover")
        self.assertEqual(calls[0][1],["discover",self.config["catalog_root"]])
        self.assertFalse((self.new/"plans/main").exists())
        pointer=RUN.read_json(self.new/"reports/generation-admission-current.json")
        self.assertEqual(RUN.read_json(self.new/pointer["attempt"])["status"],"failed")

    def admission_runner(self, failed_command):
        from types import SimpleNamespace
        proxy=object.__new__(RUN.Runner)
        proxy.root=self.new
        proxy.config=self.config
        proxy.binding=self.runner.binding
        proxy.generation=SimpleNamespace(receipt={"original_inventory":self.inventory},replay=lambda *_:None)
        proxy.binary=self.new/"fixture-child"
        proxy.binary.write_text("#!/usr/bin/env python3\nimport json,sys\nif sys.argv[1]=="+repr(failed_command)+": sys.exit(7)\nprint("+repr(json.dumps(self.inventory))+ ")\n")
        proxy.binary.chmod(0o700)
        proxy.binary_revision=RUN.revision(proxy.binary.stat())
        RUN.durable_json(self.new/"journal.json",{"next_command":1})
        return proxy

    def test_failed_discovery_cannot_retry_with_an_advanced_journal_key(self):
        proxy=self.admission_runner("discover")
        with self.assertRaises(RuntimeError):proxy.main_phase()
        journal=RUN.read_json(self.new/"journal.json")
        self.assertEqual(journal["next_command"],2)
        with self.assertRaises(RUN.InterruptedOperation):proxy.main_phase()
        self.assertEqual(RUN.read_json(self.new/"journal.json"),journal)
        self.assertEqual(len(list((self.new/"commands").iterdir())),1)

    def test_failed_registration_cannot_retry_after_another_discovery(self):
        proxy=self.admission_runner("register-inventory")
        with self.assertRaises(RuntimeError):proxy.main_phase()
        journal=RUN.read_json(self.new/"journal.json")
        self.assertEqual(journal["next_command"],3)
        with self.assertRaises(RUN.InterruptedOperation):proxy.main_phase()
        self.assertEqual(RUN.read_json(self.new/"journal.json"),journal)
        self.assertEqual(len(list((self.new/"commands").iterdir())),2)

    def test_orphan_admission_before_step_publication_blocks_new_child(self):
        proxy=self.admission_runner("discover")
        # Simulate the persisted state after journal reservation but before a
        # command directory/started/step is published. No child success is invented.
        RUN.durable_json(self.new/"journal.json",{"next_command":2},replace=True)
        RUN.durable_json(self.new/"reports/generation-admission-orphan.json",{"status":"started","source_binding":proxy.binding})
        RUN.durable_json(self.new/"reports/generation-admission-current.json",{"attempt":"reports/generation-admission-orphan.json"})
        with self.assertRaises(RUN.InterruptedOperation):proxy.main_phase()
        self.assertEqual(RUN.read_json(self.new/"journal.json")["next_command"],2)
        self.assertEqual(list((self.new/"commands").iterdir()),[])
