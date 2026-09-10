import copy
import math
import json
import pathlib
import tempfile
from types import SimpleNamespace
from unittest import mock
import unittest
import organization_campaign as campaign

class CampaignTests(unittest.TestCase):
    def test_fast_nonoverlapping_saves_cannot_hide_background_contention(self):
        measures={name:campaign.distribution([5.0]*100) for name in ('rating','label','snapshot_browse')}
        measures['overlap_diagnostic']={name:dict(overlapping_n=20,nonoverlapping_n=180,
            overlapping=campaign.distribution([100.0]*20),nonoverlapping=campaign.distribution([5.0]*180))
            for name in ('writes','snapshot_browse')}
        self.assertTrue(all(campaign.transition_budgets(measures).values()))
        bad=copy.deepcopy(measures)
        bad['overlap_diagnostic']['writes']['overlapping']=campaign.distribution([101.0]*20)
        gates=campaign.transition_budgets(bad)
        self.assertTrue(gates['rating'] and gates['label'])
        self.assertFalse(gates['overlapping_writes'])
        for name in ('writes','snapshot_browse'):
            bad=copy.deepcopy(measures);bad['overlap_diagnostic'][name].update(overlapping_n=0,overlapping=None)
            self.assertFalse(campaign.transition_budgets(bad)[f'overlapping_{name}'])

    def test_full_page_budgets_include_fresh_percentile_and_separate_startup(self):
        trials = [dict(kind="warm", index=0, elapsed_samples_ms=[100.0]*100,
                       observer=dict(elapsed_ms=11000, rss_peak_bytes=4*1024**3))]
        trials += [dict(kind="fresh", index=i, elapsed_samples_ms=[500.0],
                        observer=dict(elapsed_ms=900.0, rss_peak_bytes=1024)) for i in range(20)]
        evidence = campaign.query_budgets(trials)
        self.assertTrue(all(evidence["passes"].values()))
        bad = copy.deepcopy(trials)
        for trial in bad[1:]: trial["elapsed_samples_ms"] = [501.0]
        self.assertFalse(campaign.query_budgets(bad)["passes"]["fresh_page"])
        self.assertTrue(campaign.query_budgets(bad)["passes"]["fresh_child_startup_guard"])
        # One 900ms outlier leaves interpolated p95=500ms; two exceed it.
        trials[1]["elapsed_samples_ms"] = [900.0]
        for trial in trials[2:]: trial["elapsed_samples_ms"] = [478.94736842105266]
        self.assertAlmostEqual(campaign.query_budgets(trials)["measurements"]["fresh_page_ms"]["p95"], 500)
        trials[2]["elapsed_samples_ms"] = [900.0]
        self.assertFalse(campaign.query_budgets(trials)["passes"]["fresh_page"])
        bad = copy.deepcopy(trials);bad[0]["elapsed_samples_ms"] = [100.001]*100
        self.assertFalse(campaign.query_budgets(bad)["passes"]["warm_page"])
        bad[1]["observer"]["elapsed_ms"] = 1000
        self.assertFalse(campaign.query_budgets(bad)["passes"]["fresh_child_startup_guard"])
        bad[1]["observer"]["rss_peak_bytes"] = 4*1024**3+1
        self.assertFalse(campaign.query_budgets(bad)["passes"]["browse_memory"])
        for mutation in [lambda t:t.pop(),lambda t:t[1].update(index=2),
                         lambda t:t[1].update(elapsed_samples_ms=[]),
                         lambda t:t[1].update(elapsed_samples_ms=[float('nan')])]:
            bad=copy.deepcopy(trials);mutation(bad)
            with self.assertRaises((AssertionError,ValueError)):campaign.query_budgets(bad)

    def receipt(self):
        rows=[]
        for i in range(501,701):
            rows.append(dict(sequence=i,asset_id=f"fixture-{i:012}",state="ready",metadata_revision=0,folder=2+i%5,filename=f"file{i:012}.jpg",capture=f"2024-01-{i%28+1:02}T12:00:00",camera_make="fixture",camera=f"camera{i%3}",lens=f"lens{i%4}",format="JPEG" if i%2==0 else "DNG",rating=i%6,flag=["reject","pick","unflagged"][i%3],label="red" if i%2==0 else "blue",conflicts=["gps_latitude"] if i%97==0 else [],provenance={"synthetic_fixture":1}))
        anchor=dict(version=1,epoch=0,high_water=1000,sequence=500,key=dict(kind="integer",value=500))
        sample=dict(iteration=0,anchor=anchor,elapsed_ms=20.0,error=None,rows=rows,oracle_sequences=list(range(501,701)),chunks=[dict(scanned=200,returned=200,sorts=0,vm_steps=4000,elapsed_ms=19.0,exhausted=False,has_more=True,cursor=dict(sequence=700),text_work=dict(candidate_rows_read=200,indexed_rows=0,indexed_bytes=0,batches=0,vm_steps=0,sorts=0,admission_limited=False))])
        receipt=dict(protocol=2,catalog_schema=6,complete=True,mode="query",count=1000,case="browse",repetitions=1,warmups=0,start=0,errors=[],plans=["SEARCH"],settings=dict(diagnostic_shared_helper_connection=campaign.SETTINGS),engine_version="3.51.1",text_limits=campaign.TEXT_LIMITS,open_ms=2.0,samples=[sample],warmup_samples=[])
        observer=dict(exit_code=0,error=None,rss_samples=1,rss_peak_bytes=1024,elapsed_ms=50)
        return receipt,observer
    def test_closed_gates_reject_false_pages_settings_missing_memory_and_errors(self):
        valid,observer=self.receipt()
        self.assertEqual(campaign.validate_receipt(valid,observer,"browse",1000,1,0,0)["n"],1)
        for change in [lambda r:r.update(complete=False), lambda r:r.update(samples=[]), lambda r:r.update(plans=[]), lambda r:r["settings"].update(diagnostic_shared_helper_connection={}), lambda r:r["samples"][0].update(rows=[],oracle_sequences=[]), lambda r:r["samples"][0]["rows"][0].update(label="wrong"), lambda r:r["samples"][0].update(elapsed_ms=float('nan')), lambda r:r["samples"][0]["chunks"][0].update(sorts=1), lambda r:r["samples"][0].update(error="native error")]:
            receipt=copy.deepcopy(valid);change(receipt)
            with self.assertRaises((AssertionError,ValueError)):
                campaign.validate_receipt(receipt,observer,"browse",1000,1,0,0)
        bad=copy.deepcopy(observer);bad["rss_peak_bytes"]=None
        with self.assertRaises(AssertionError):campaign.validate_receipt(valid,bad,"browse",1000,1,0,0)
    def test_interpolated_quantiles_and_nonfinite_rejection(self):
        self.assertAlmostEqual(campaign.distribution([1,2,3,4])["p95"],3.85)
        for values in [[],[-1],[True],[float('nan')],[float('inf')],["1"]]:
            with self.assertRaises(ValueError):campaign.distribution(values)
    def test_oracle_sparse_tails_cannot_be_claimed_empty(self):
        anchor=dict(sequence=500_000,key=dict(kind="integer",value=500_000))
        self.assertEqual(campaign.expected_sequences("wide-keyword",1_000_000,anchor),list(range(501_000,1_000_001,10_000)))
        self.assertEqual(campaign.expected_sequences("mixed",1_000,dict(sequence=500,key=dict(kind="integer",value=500))),[820])

    def test_text_counter_contract_rejects_omission_hidden_work_and_wrong_admission(self):
        valid, observer = self.receipt()
        changes = [
            lambda r:r.pop("text_limits"),
            lambda r:r.update(text_limits={"document_bytes":1,"page_bytes":1}),
            lambda r:r["samples"][0]["chunks"][0].pop("text_work"),
            lambda r:r["samples"][0]["chunks"][0]["text_work"].update(candidate_rows_read=201),
            lambda r:r["samples"][0]["chunks"][0]["text_work"].update(indexed_rows=1),
            lambda r:r["samples"][0]["chunks"][0]["text_work"].update(vm_steps=1),
            lambda r:r["samples"][0]["chunks"][0]["text_work"].update(admission_limited=True),
            lambda r:r["samples"][0]["chunks"][0].update(elapsed_ms=21),
            lambda r:r.update(plans=["SCAN organization_text VIRTUAL TABLE INDEX 0:=M1"]),
        ]
        for change in changes:
            item=copy.deepcopy(valid);change(item)
            with self.assertRaises((AssertionError,KeyError)):
                campaign.validate_receipt(item,observer,"browse",1000,1,0,0)
        local=dict(scanned=400,returned=80,vm_steps=20000,text_work=dict(candidate_rows_read=400,indexed_rows=400,indexed_bytes=5200,batches=4,vm_steps=9000,sorts=0,admission_limited=False))
        campaign.validate_text_work(local,True)
        for field,value in [("indexed_rows",401),("indexed_rows",79),("batches",3),("indexed_bytes",6000),("indexed_bytes",4000),("vm_steps",20001),("vm_steps",0),("candidate_rows_read",399),("sorts",1),("indexed_rows",True)]:
            bad=copy.deepcopy(local);bad["text_work"][field]=value
            with self.assertRaises(AssertionError):campaign.validate_text_work(bad,True)

    def reusable(self, base):
        # Header-only bytes exercise filesystem/provenance checks, not native DB
        # validity. Native query startup separately checks fixture protocol/high.
        old=base/"preserved";old.mkdir()
        catalog=old/"catalog-1000";catalog.mkdir()
        header=bytearray(100);header[:16]=b"SQLite format 3\0"
        header[60:64]=(4).to_bytes(4,"big");header[68:72]=(0x50484341).to_bytes(4,"big")
        main=catalog/"catalog.sqlite3";main.write_bytes(header)
        (catalog/"catalog.sqlite3-wal").write_bytes(b"")
        (catalog/"catalog.sqlite3-shm").write_bytes(bytes(32768))
        prep=dict(protocol=1,complete=True,mode="prepare",count=1000,engine_version="3.51.1",settings={"diagnostic_shared_helper_connection":campaign.SETTINGS},counts=[[name,1000*m] for name,m in (("assets",1),("organization_assets",1),("organization_keyword_members",4),("organization_folder_members",2),("organization_text",1))])
        prepare=old/"prepare-1000.json";campaign.save(prepare,prep)
        fixture=dict(count=1000,catalog=str(catalog),main_sha256=campaign.sha(main),main_bytes=100,prepare_receipt_sha256=campaign.sha(prepare))
        manifest=old/"manifest.json";campaign.save(manifest,dict(protocol=1,complete=True,smoke=True,scales=[1000],fixtures=[fixture]))
        root=base/"new";root.mkdir()
        args=SimpleNamespace(root=root,reuse_prepared=manifest,smoke=True)
        return args,fixture,main

    def test_reuse_preserves_exact_sources_companions_and_receipt_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            args,fixture,main=self.reusable(pathlib.Path(directory))
            before=campaign.source_state(main);manifest_before=args.reuse_prepared.read_bytes()
            with mock.patch.object(campaign,"migrate_reused_fixture",side_effect=lambda args,fixture:fixture):
                campaign.prepare_reuse(args,[1000])
            result=json.loads((args.root/"manifest.json").read_text())
            self.assertTrue(result["complete"])
            self.assertEqual(campaign.source_state(main),before)
            self.assertEqual(args.reuse_prepared.read_bytes(),manifest_before)
            target=pathlib.Path(result["fixtures"][0]["catalog"])/"catalog.sqlite3"
            self.assertEqual(target.read_bytes(),main.read_bytes())
            self.assertEqual(list(target.parent.glob("catalog.sqlite3-*")),[])
            proof=json.loads((args.root/"reuse-1000.json").read_text())
            self.assertEqual(proof["source_before"],proof["source_after"])
            self.assertEqual(proof["copy_sha256"],fixture["main_sha256"])
            self.assertEqual(campaign.sha(args.root/"prepare-1000.json"),fixture["prepare_receipt_sha256"])
            with self.assertRaises(FileExistsError):campaign.prepare_reuse(args,[1000])

    def schema5_reusable(self, base):
        args,fixture,main=self.reusable(base)
        schema4=fixture['main_sha256']
        header=bytearray(main.read_bytes());header[60:64]=(5).to_bytes(4,'big');main.write_bytes(header)
        fixture.update(schema=5,source_main_sha256=schema4,main_sha256=campaign.sha(main))
        tables=[['assets',1000],['organization_assets',1000],['organization_text',1000],['organization_text_idx',12],['storage_bindings',0]]
        native=dict(protocol=1,complete=True,mode='migrate_fixture',count=1000,engine_version='3.51.1',schema_before=4,schema_after=5,
                    logical_before='a'*64,logical_after='a'*64,table_counts_before=tables,table_counts_after=tables,
                    index_sql='CREATE INDEX organization_lens_capture ON organization_assets(lens,capture,sequence)')
        native_path=args.reuse_prepared.parent/'migrate-1000.json';campaign.save(native_path,native)
        proof={k:native[k] for k in ['schema_before','schema_after','logical_before','logical_after','table_counts_before','table_counts_after','index_sql']}
        proof.update(owned_copy_before_sha256=schema4,owned_copy_after_sha256=fixture['main_sha256'],native_receipt=str(native_path),
                     native_receipt_sha256=campaign.sha(native_path),observer=dict(exit_code=0,error=None))
        proof_path=args.reuse_prepared.parent/'migration-proof-1000.json';campaign.save(proof_path,proof)
        fixture['migration_proof']=str(proof_path);args.binary=pathlib.Path('unused-binary')
        return args,fixture,main,native

    def test_schema5_reuse_retains_ancestor_bytes_and_migrates_to_six_separately(self):
        with tempfile.TemporaryDirectory() as directory:
            args,fixture,main,native=self.schema5_reusable(pathlib.Path(directory))
            before=campaign.source_state(main)
            copied=campaign.copied_fixture(args,args.reuse_prepared,fixture)
            ancestor=copied['prior_migration']
            self.assertEqual(pathlib.Path(ancestor['proof']).read_bytes(),pathlib.Path(fixture['migration_proof']).read_bytes())
            self.assertEqual(campaign.sha(ancestor['native']),ancestor['native_sha256'])
            native.update(protocol=2,catalog_schema=6,schema_before=5,schema_after=6,identity_scope="pre_existing_tables",added_tables=campaign.schema6_initial_rows(native["table_counts_before"]),alias_initial_state=campaign.schema6_alias_state(native["table_counts_before"]))
            def child(binary,catalog,output,argv,timeout):
                self.assertEqual(argv,['migrate-fixture']);campaign.save(output,native)
                target=catalog/'catalog.sqlite3';header=bytearray(target.read_bytes());header[60:64]=(6).to_bytes(4,'big');target.write_bytes(header)
                return dict(exit_code=0,error=None)
            with mock.patch.object(campaign,'run_child',side_effect=child):
                result=campaign.migrate_reused_fixture(args,copied)
            proof=json.loads(pathlib.Path(result['migration_proof']).read_text())
            self.assertEqual(proof['kind'],'schema5_to_6_migration')
            self.assertNotEqual(proof['owned_copy_before_sha256'],proof['owned_copy_after_sha256'])
            self.assertEqual(result['prior_migration'],ancestor)
            self.assertNotEqual(ancestor['schema4_main_sha256'],result['main_sha256'])
            self.assertEqual(campaign.source_state(main),before)
            with self.assertRaises(FileExistsError):campaign.preserve_schema5_ancestry(args,args.reuse_prepared,fixture)

    def test_current_reuse_preserves_previous_migration_proof_and_rejects_substitution(self):
        with tempfile.TemporaryDirectory() as directory:
            args,fixture,main,native=self.schema5_reusable(pathlib.Path(directory))
            header=bytearray(main.read_bytes());header[60:64]=(6).to_bytes(4,"big");main.write_bytes(header)
            fixture.update(schema=6,main_sha256=campaign.sha(main),source_main_sha256="b"*64)
            native.update(protocol=2,catalog_schema=6,schema_before=5,schema_after=6,identity_scope="pre_existing_tables",added_tables=campaign.schema6_initial_rows(native["table_counts_before"]),alias_initial_state=campaign.schema6_alias_state(native["table_counts_before"]))
            native_path=args.reuse_prepared.parent/"schema6-native.json";campaign.save(native_path,native)
            proof={**native,"native_receipt":str(native_path),"native_receipt_sha256":campaign.sha(native_path),"owned_copy_before_sha256":"b"*64,"owned_copy_after_sha256":fixture["main_sha256"],"observer":{"exit_code":0,"error":None}}
            proof_path=args.reuse_prepared.parent/"schema6-proof.json";campaign.save(proof_path,proof)
            fixture["migration_proof"]=str(proof_path)
            retained=campaign.preserve_current_ancestry(args,args.reuse_prepared,fixture)
            self.assertEqual(pathlib.Path(retained["proof"]).read_bytes(),proof_path.read_bytes())
            self.assertEqual(pathlib.Path(retained["native"]).read_bytes(),native_path.read_bytes())
            self.assertEqual(retained["predecessor_manifest_sha256"],campaign.sha(args.reuse_prepared))
            proof["owned_copy_after_sha256"]="f"*64;proof_path.write_text(json.dumps(proof))
            with self.assertRaises(AssertionError):
                campaign.preserve_current_ancestry(args,args.reuse_prepared,fixture)

    def test_schema5_ancestry_rejects_changed_native_index_logical_or_physical_claim(self):
        for failure in ['native_bytes','index','logical','physical','schema']:
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as directory:
                args,fixture,main,native=self.schema5_reusable(pathlib.Path(directory))
                proof_path=pathlib.Path(fixture['migration_proof']);proof=json.loads(proof_path.read_text())
                if failure=='native_bytes':pathlib.Path(proof['native_receipt']).write_text('{}')
                elif failure=='index':proof['index_sql']='CREATE INDEX wrong ON assets(id)'
                elif failure=='logical':proof['logical_after']='b'*64
                elif failure=='physical':proof['owned_copy_after_sha256']='b'*64
                else:proof['schema_before']=5
                proof_path.write_text(json.dumps(proof))
                with self.assertRaises(AssertionError):campaign.preserve_schema5_ancestry(args,args.reuse_prepared,fixture)
                self.assertFalse((args.root/'ancestry-1000').exists())

    def test_reuse_rejects_dirty_wrong_schema_changed_source_or_prepare_counts(self):
        for failure in ("wal","schema","hash","counts","overlap"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                args,fixture,main=self.reusable(pathlib.Path(directory))
                if failure=="wal":main.with_name(main.name+"-wal").write_bytes(b"pending")
                elif failure=="schema":
                    contents=bytearray(main.read_bytes());contents[60:64]=(3).to_bytes(4,"big");main.write_bytes(contents)
                    fixture["main_sha256"]=campaign.sha(main)
                elif failure=="hash":main.write_bytes(main.read_bytes()+b"changed")
                elif failure=="counts":
                    path=args.reuse_prepared.parent/"prepare-1000.json"
                    data=json.loads(path.read_text());data["counts"][0][1]=999;path.write_text(json.dumps(data))
                    fixture["prepare_receipt_sha256"]=campaign.sha(path)
                else:args.root=main.parent/"nested"
                with self.assertRaises(AssertionError):campaign.copied_fixture(args,args.reuse_prepared,fixture)
                self.assertTrue(main.exists())
                self.assertFalse((args.root/"reuse-1000.json").exists())

    def test_copy_time_mutation_fails_without_reverting_external_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            args,fixture,main=self.reusable(pathlib.Path(directory))
            original=campaign.shutil.copyfileobj
            def change_source(incoming,outgoing,length):
                original(incoming,outgoing,length)
                main.write_bytes(main.read_bytes()+b"external update")
            with mock.patch.object(campaign.shutil,"copyfileobj",side_effect=change_source):
                with self.assertRaises(SystemExit):campaign.prepare_reuse(args,[1000])
            self.assertTrue(main.read_bytes().endswith(b"external update"))
            result=json.loads((args.root/"manifest.json").read_text())
            self.assertFalse(result["complete"])
            self.assertEqual(result["fixtures"],[])
            self.assertIn("source changed while copying",result["error"])
            self.assertTrue((args.root/"catalog-1000/catalog.sqlite3").exists())

    def test_diagnostic_dispatch_fixed_matrix_retains_failure_and_is_not_acceptance(self):
        with tempfile.TemporaryDirectory() as directory:
            root=pathlib.Path(directory);fixtures=[]
            for count in campaign.SCALES:
                catalog=root/f"catalog-{count}";catalog.mkdir();main=catalog/"catalog.sqlite3";main.write_bytes(b"unchanged fixture")
                fixtures.append(dict(count=count,catalog=str(catalog),main_sha256=campaign.sha(main)))
            args=SimpleNamespace(root=root,binary=pathlib.Path("unused-binary"),smoke=False)
            calls=[]
            def child(binary,catalog,output,argv,timeout):
                calls.append((int(catalog.name.split("-")[-1]),argv,timeout))
                campaign.save(output,{"placeholder":True})
                return dict(exit_code=0,error=None,rss_samples=1,rss_peak_bytes=1024)
            validation=[ValueError("retained first failure")]+[{} for _ in range(7)]
            with mock.patch.object(campaign,"run_child",side_effect=child), mock.patch.object(campaign,"validate_receipt",side_effect=validation):
                with self.assertRaises(SystemExit):campaign.run_diagnostic(args,{"fixtures":fixtures})
            self.assertEqual([(n,argv[1]) for n,argv,_ in calls],[(n,c) for n in [1000000,10000000] for c in ["filename-reverse","text","text-capture","date-camera"]])
            self.assertTrue(all(argv[2:]==["--repetitions","5","--warmups","3","--start","0"] and timeout==120 for _,argv,timeout in calls))
            result=json.loads((root/"diagnostic/diagnostic.json").read_text())
            self.assertFalse(result["acceptance_evidence"])
            self.assertFalse(result["complete"])
            self.assertEqual(len(result["errors"]),1)
            self.assertEqual(len(result["cases"]),7)
            self.assertEqual(len(result["source_proofs"]),2)
            self.assertTrue(all(p["unchanged"] for p in result["source_proofs"]))

    def test_capture_lens_empty_tail_matches_oracle_without_false_200_requirement(self):
        receipt,observer=self.receipt()
        receipt.update(count=1_000_000,case="text_capture",start=1)
        sample=receipt["samples"][0]
        sample.update(iteration=1,rows=[],oracle_sequences=[])
        sample["anchor"].update(high_water=1_000_000,sequence=545000,key={"kind":"text","value":"2024-01-26T12:00:00"})
        sample["chunks"]=[dict(scanned=0,returned=0,sorts=0,vm_steps=100,elapsed_ms=10.,exhausted=True,has_more=False,cursor=None,text_work=dict(candidate_rows_read=0,indexed_rows=0,indexed_bytes=0,batches=0,vm_steps=10,sorts=0,admission_limited=False))]
        self.assertEqual(campaign.validate_receipt(receipt,observer,"text-capture",1_000_000,1,0,1)["n"],1)
        bad=copy.deepcopy(receipt);bad["samples"][0]["chunks"][0].update(exhausted=False,has_more=True)
        with self.assertRaises(AssertionError):campaign.validate_receipt(bad,observer,"text-capture",1_000_000,1,0,1)
        # Day15 has matching later dates: falsely returning empty must still fail.
        bad=copy.deepcopy(receipt);bad.update(start=0);bad["samples"][0].update(iteration=0)
        bad["samples"][0]["anchor"].update(sequence=500000,key={"kind":"text","value":"2024-01-15T12:00:00"})
        with self.assertRaises(AssertionError):campaign.validate_receipt(bad,observer,"text-capture",1_000_000,1,0,0)

class CurrentSchemaContracts(unittest.TestCase):
    def test_migration_scope_additions_and_old_protocol_reject(self):
        tables=[["assets",1000],["organization_assets",1000],["organization_text",1000],["storage_bindings",7]]
        native=dict(protocol=2,catalog_schema=6,complete=True,mode="migrate_fixture",count=1000,
                    schema_before=5,schema_after=6,identity_scope="pre_existing_tables",
                    logical_before="a"*64,logical_after="a"*64,table_counts_before=tables,table_counts_after=tables,
                    added_tables=campaign.schema6_initial_rows(tables),alias_initial_state={"unbound":993,"dirty":7},
                    index_sql="CREATE INDEX organization_lens_capture ON organization_assets(lens,capture,sequence)")
        self.assertEqual(dict(native["added_tables"])["export_alias_state"],1)
        self.assertEqual(dict(native["added_tables"])["export_alias_dirty"],7)
        self.assertEqual(dict(native["added_tables"])["export_alias_paths"],0)
        campaign.validate_current_migration(native,1000,5)
        for field,value in [("protocol",1),("catalog_schema",5),("schema_after",5),("identity_scope","all_tables"),("logical_after","b"*64),("added_tables",[]),("added_tables",[[name,0] for name in campaign.SCHEMA6_TABLES]),("alias_initial_state",{"unbound":1000,"dirty":0})]:
            with self.subTest(field=field), self.assertRaises(AssertionError):
                campaign.validate_current_migration({**native,field:value},1000,5)
        current={**native,"schema_before":6,"added_tables":[],"table_counts_before":tables+campaign.schema6_initial_rows(tables),"table_counts_after":tables+campaign.schema6_initial_rows(tables)}
        campaign.validate_current_migration(current,1000,6)
        with self.assertRaises(AssertionError):
            campaign.validate_current_migration({**current,"added_tables":native["added_tables"]},1000,6)

if __name__=="__main__":unittest.main()
