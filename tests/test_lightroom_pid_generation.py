"""Protocol3 fixtures: a real tiny schema2 clone; no user catalogs or native CLI."""
import copy
import importlib.util
import json
from pathlib import Path
import shutil
import time
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("repair_fixture", Path(__file__).with_name("test_lightroom_repair_generation.py"))
BASE = importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(BASE)
RUN, GEN = BASE.RUN, BASE.GEN


class PidGenerationTests(unittest.TestCase):
    def setUp(self):
        self.base = BASE.RepairGenerationTests(); self.base.setUp(); f = self.f = self.base.f
        self.v2, self.v4 = self.base.ancestor, f.old
        # Complete the fixture's original v4 upgrade receipts with exact typed
        # before/after evidence; the real predecessor already has these files.
        before = GEN.typed_scan(self.v2/"plans/main/inspection.sqlite3", f.request["limits"])
        after = GEN.typed_scan(self.v4/"plans/main/inspection.sqlite3", f.request["limits"])
        RUN.durable_json(self.v4/"reports/generation-before.json", before)
        RUN.durable_json(self.v4/"reports/generation-after.json", after)
        old_receipt = RUN.read_json(self.v4/"reports/generation-adoption.json")
        old_receipt.update(before_sha256=RUN.file_sha(self.v4/"reports/generation-before.json"), after_sha256=RUN.file_sha(self.v4/"reports/generation-after.json"))
        RUN.durable_json(self.v4/"reports/generation-adoption.json", old_receipt, replace=True)
        f.request["inherited_adoption"] = f.reference("reports/generation-adoption.json")
        f.runner.binding = {"source":"v5 fixture", "binary_sha256":"synthetic", "config_sha256":RUN.sha(RUN.encoded(f.config))}
        RUN.durable_json(f.new/"config.json", f.config)
        RUN.durable_json(f.new/"binding.json", f.runner.binding)
        (f.new/"runner.lock").touch()
        GEN.adopt(f.runner)
        replay = GEN.RepairReplay(f.runner)
        try: done = replay.outcome(RUN.candidate_key(f.candidates[0]), f.candidates[0])
        finally: replay.close()
        self.v5 = f.new
        f.old = self.v5; f.new = f.root/"v6"
        for name in ["commands","steps","reports","plans","captures"]: (f.new/name).mkdir(parents=True)
        f.binding = copy.deepcopy(f.runner.binding)
        f.config = copy.deepcopy(f.config)
        f.next = 1
        inventory = f.command(["v5","discover"], ["discover",f.config["catalog_root"]], f.inventory)
        good = f.command(f.active_logical_key,["rows",str(f.old/"plans/main"),"active","--after","0","--limit","1"], f.values["active"][:1])
        self.failed_key = [["main",f.active_key],"active","rows",1]
        bad = f.command(self.failed_key,["rows",str(f.old/"plans/main"),"active","--after","3","--limit","1"], f.values["active"][1:])
        for sequence, started, finished in [(2,100,101),(3,400,401)]:
            directory=f.old/"commands"/f"{sequence:09d}";record=RUN.read_json(directory/"result.json")
            record.update(argv=[str(f.old/"lightroom_inspect")]+record["requested_arguments"],started_unix=started,finished_unix=finished)
            if sequence == 3:record.update(exit_code=-9,failure="interrupted_or_launch_error: KeyboardInterrupt()")
            (directory/"stderr").write_bytes(b"fixture interrupted\n" if sequence==3 else b"")
            record["stderr"]={"path":str((directory/"stderr").relative_to(f.old)),"sha256":RUN.file_sha(directory/"stderr"),"bytes":(directory/"stderr").stat().st_size}
            RUN.durable_json(directory/"result.json",record,replace=True)
            RUN.durable_json(directory/"process.json",{"pid":12345,"process_group":12345,"argv":record["argv"],"started_unix":started+0.1})
        RUN.durable_json(f.old/"journal.json",{"next_command":4})
        RUN.durable_json(f.old/"reports/completed.json",done)
        self.control=f.root/"v5-control";self.attempt=self.control/"attempts/00000000-0000-0000-0000-000000000005";self.attempt.mkdir(parents=True)
        failed={"status":"failed_or_unknown","exit_code":1,"failure":"RuntimeError('owned PID identity changed')","ownership_status":"unknown_requires_review","started_unix":90,"finished_unix":402,
                "cleanup":{"root_reaped":True,"remaining_observed":[],"ownership_uncertainties":[],"errors":["initial process observation: RuntimeError('owned PID identity changed')"],"root_signals":["SIGINT"]}}
        RUN.durable_json(self.attempt/"result.json",failed)
        RUN.durable_json(self.attempt/"process.json",{"pid":54321,"argv":["python","runner.py","main",str(f.old)],"started_unix":90})
        RUN.durable_json(self.attempt/"stop-decision.json",{"root_pid":54321,"reason":failed["failure"],"unix":400.5,"known_owned":[{"pid":12345,"group":12345,"parent":54321,"start":time.strftime('%a %b %d %H:%M:%S %Y',time.localtime(100))}]})
        RUN.durable_json(self.control/"current.json",{"attempt_id":self.attempt.name})
        RUN.durable_json(f.old/"reports/failed-phase.json",{"phase":"main","status":"failed_or_interrupted","binding":f.binding,"started_unix":91,"finished_unix":401.5,"error":"InterruptedOperation('interrupted_or_launch_error: KeyboardInterrupt()')"})
        self.request = f.request = copy.deepcopy(f.config["generation_request"])
        tail={"record":f.reference(bad['path']),"process":f.reference("commands/000000003/process.json"),"prior_record":f.reference(good['path']),"prior_process":f.reference("commands/000000002/process.json"),"stop_decision":self.absolute(self.attempt/"stop-decision.json"),"root_process":self.absolute(self.attempt/"process.json")}
        prefix={RUN.sha(RUN.encoded(RUN.read_json(f.old/f"commands/{n:09d}/result.json")["key"])):f.reference(f"commands/{n:09d}/result.json") for n in [1,2]}
        self.request.update(protocol=3,predecessor=str(f.old),binding=f.reference("binding.json"),journal=f.reference("journal.json"),expected_next_command=4,command_index_sha256=RUN.sha(RUN.encoded(prefix)),
            inventory_result=inventory,inherited_adoption=f.reference("reports/generation-adoption.json"),completed_outcomes=[f.reference("reports/completed.json")],expected_source_state=GEN.artifact_state(f.old/"plans/main"),
            interrupted_tail=tail,page_limit=1,failed_predecessor={"result":self.absolute(self.attempt/"result.json"),"phase":self.absolute(f.old/"reports/failed-phase.json"),"control":self.absolute(self.control/"current.json"),"ownership_review":None})
        review={"status":"PASS","failed_result_sha256":self.request['failed_predecessor']['result']['sha256'],"phase_sha256":self.request['failed_predecessor']['phase']['sha256'],"interrupted_tail_sha256":RUN.sha(RUN.encoded(tail)),"next_command":4,"known_processes_absent":True,"no_unresolved_command":True,"terminal_failed_commands":1,"terminal_success_commands":2}
        RUN.durable_json(self.control/"ownership-review.json",review)
        self.request['failed_predecessor']['ownership_review']=self.absolute(self.control/"ownership-review.json")
        f.config['generation_request']=self.request
        f.runner.root=f.new;f.runner.config=f.config;f.runner.binding={"source":"v6 fixture"}

    def tearDown(self):self.base.tearDown()
    def absolute(self,path):return {"path":str(path),"sha256":RUN.file_sha(path)}
    def mutate(self,path,change):
        value=RUN.read_json(path);change(value);RUN.durable_json(path,value,replace=True)
    def tree(self):return {str(p):RUN.file_sha(p) for root in [self.v2,self.v4,self.v5,self.control,self.base.control] for p in root.rglob('*') if p.is_file()}

    def test_three_origins_and_failed_tail_preserved_but_never_replayed(self):
        before=self.tree()
        with patch.object(GEN,'typed_scan',wraps=GEN.typed_scan) as scan:receipt=RUN.read_json(GEN.adopt(self.f.runner))
        self.assertEqual(scan.call_count,1);self.assertEqual(before,self.tree());self.assertEqual(receipt['protocol'],3);self.assertEqual(set(receipt['origins']),{'v5','v4','v2'})
        self.assertEqual(receipt['failure_proof']['failed_page_index'],1);self.assertEqual(receipt['active_verified_prefix']['rows'],1)
        self.assertEqual(list((self.f.new/'commands').iterdir()),[]);self.assertEqual(self.f.calls,[])
        replay=GEN.PidRepairReplay(self.f.runner)
        try:
            outcome=replay.outcome(RUN.candidate_key(self.f.candidates[0]),self.f.candidates[0])
            self.assertEqual(outcome['inspection']['report'],'adopted/v2/000000002')
            self.assertEqual(replay.document('adopted/v4/000000002','rows'),self.f.values['active'][:1])
            result=replay.replay(self.f.active_logical_key,['rows',self.f.new/'plans/main','active','--after',0,'--limit',1])
            self.assertEqual(result['record']['sequence'],'adopted/v5/000000002')
            self.assertIsNone(replay.replay(self.failed_key,['rows',self.f.new/'plans/main','active','--after',3,'--limit',1]))
            with self.assertRaises(ValueError):replay.reference('adopted/v5/000000003')
            for invalid in ['adopted/2','adopted/v4/2','adopted/v4/00000000²',True]:
                with self.subTest(invalid=invalid),self.assertRaises(ValueError):replay.reference(invalid)
        finally:replay.close()

    def test_any_other_supervisor_error_is_not_admitted(self):
        self.mutate(self.attempt/'result.json',lambda x:x.update(failure="RuntimeError('other')"));self.request['failed_predecessor']['result']=self.absolute(self.attempt/'result.json')
        with self.assertRaisesRegex(ValueError,'reviewed PID-reuse'):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/'plans/main').exists())

    def test_failed_mutating_command_cannot_be_excluded(self):
        p=self.v5/'commands/000000003/result.json';self.mutate(p,lambda x:x['requested_arguments'].__setitem__(0,'resume'));self.request['interrupted_tail']['record']=self.f.reference('commands/000000003/result.json')
        with self.assertRaisesRegex(ValueError,'read-only tail'):GEN.adopt(self.f.runner)

    def test_partial_failed_stdout_digest_and_unresolved_process_are_required(self):
        (self.v5/'commands/000000003/stdout').write_bytes(b'[{"partial":')
        with self.assertRaisesRegex(ValueError,'raw stream changed'):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/'plans/main').exists())

    def test_missing_tail_process_is_not_a_qualified_failure(self):
        (self.v5/'commands/000000003/process.json').unlink()
        with self.assertRaises((ValueError,FileNotFoundError)):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/'plans/main').exists())

    def test_prior_command_must_have_ended_before_reused_pid(self):
        p=self.v5/'commands/000000002/result.json';self.mutate(p,lambda x:x.update(finished_unix=400.5));self.request['interrupted_tail']['prior_record']=self.f.reference('commands/000000002/result.json')
        with self.assertRaisesRegex(ValueError,'read-only tail'):GEN.adopt(self.f.runner)

    def test_parent_microsecond_birth_not_invented_from_old_wallclock_evidence(self):
        # The OLD failed supervisor only observed seconds. Its preserved proof
        # must identify a prior lifetime several minutes earlier, not claim usec.
        p=self.attempt/'stop-decision.json';self.mutate(p,lambda x:x['known_owned'][0].update(start=time.strftime('%a %b %d %H:%M:%S %Y',time.localtime(400))))
        self.request['interrupted_tail']['stop_decision']=self.absolute(p)
        with self.assertRaisesRegex(ValueError,'distinct old observed'):GEN.adopt(self.f.runner)

    def test_parent_config_hash_and_ancestral_typed_receipts_bound(self):
        self.mutate(self.v5/'config.json',lambda x:x.update(page_limit=99))
        with self.assertRaisesRegex(ValueError,'config binding'):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/'plans/main').exists())

    def test_ancestral_typed_evidence_mutation_rejected(self):
        self.mutate(self.v4/'reports/generation-before.json',lambda x:x.update(plan_schema=99))
        with self.assertRaises(ValueError):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/'plans/main').exists())

    def test_source_companions_not_ignored(self):
        (self.v5/'plans/main/inspection.sqlite3-wal').write_bytes(b'active WAL')
        with self.assertRaisesRegex(ValueError,'nonempty WAL'):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/'plans/main').exists())

    def test_wrong_counts_and_partial_copy_never_publish_ready_or_retry(self):
        self.request['expected_rows']['active']=999
        with self.assertRaisesRegex(ValueError,'schema/count'):GEN.adopt(self.f.runner)
        self.assertTrue((self.f.new/'plans/main/inspection.sqlite3').exists());self.assertFalse((self.f.new/'reports/generation-adoption.json').exists())
        with self.assertRaises(RUN.InterruptedOperation):GEN.adopt(self.f.runner)

    def test_active_successful_cursor_not_failed_stdout_establishes_resume(self):
        self.request['active']['after']=4
        with self.assertRaisesRegex(ValueError,'read-only tail'):GEN.adopt(self.f.runner)
        self.assertFalse((self.f.new/'plans/main').exists())

    def test_replay_detects_changed_failed_evidence_without_new_native_work(self):
        GEN.adopt(self.f.runner)
        (self.v5/'commands/000000003/stderr').write_bytes(b'changed failure evidence')
        with self.assertRaisesRegex(ValueError,'raw stream changed'):GEN.PidRepairReplay(self.f.runner)
        self.assertEqual(self.f.calls,[])


if __name__=='__main__':unittest.main()
