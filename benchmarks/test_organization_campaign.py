import copy
import math
import unittest
import organization_campaign as campaign

class CampaignTests(unittest.TestCase):
    def receipt(self):
        rows=[]
        for i in range(501,701):
            rows.append(dict(sequence=i,asset_id=f"fixture-{i:012}",state="ready",metadata_revision=0,folder=2+i%5,filename=f"file{i:012}.jpg",capture=f"2024-01-{i%28+1:02}T12:00:00",camera_make="fixture",camera=f"camera{i%3}",lens=f"lens{i%4}",format="JPEG" if i%2==0 else "DNG",rating=i%6,flag=["reject","pick","unflagged"][i%3],label="red" if i%2==0 else "blue",conflicts=["gps_latitude"] if i%97==0 else [],provenance={"synthetic_fixture":1}))
        anchor=dict(version=1,epoch=0,high_water=1000,sequence=500,key=dict(kind="integer",value=500))
        sample=dict(iteration=0,anchor=anchor,elapsed_ms=20.0,error=None,rows=rows,oracle_sequences=list(range(501,701)),chunks=[dict(scanned=200,returned=200,sorts=0,vm_steps=4000,elapsed_ms=19.0,exhausted=False,has_more=True,cursor=dict(sequence=700))])
        receipt=dict(protocol=1,complete=True,mode="query",count=1000,case="browse",repetitions=1,warmups=0,start=0,errors=[],plans=["SEARCH"],settings=dict(diagnostic_shared_helper_connection=campaign.SETTINGS),engine_version="3.51.1",open_ms=2.0,samples=[sample],warmup_samples=[])
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

if __name__=="__main__":unittest.main()
