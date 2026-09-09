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
        receipt=dict(protocol=1,complete=True,mode="query",count=1000,case="browse",repetitions=1,warmups=0,start=0,errors=[],plans=["SEARCH"],settings=dict(diagnostic_shared_helper_connection=campaign.SETTINGS),engine_version="3.51.1",text_limits=campaign.TEXT_LIMITS,open_ms=2.0,samples=[sample],warmup_samples=[])
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

if __name__=="__main__":unittest.main()
