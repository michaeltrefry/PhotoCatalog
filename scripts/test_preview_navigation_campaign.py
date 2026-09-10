"""Tiny receipt-contract tests; no timing, image work, database or child process."""
import copy
import unittest
from preview_navigation_campaign import distribution, expected_trials, plan, validate_binding, validate_trial


def page():
    return {"kind":"fresh","index":0,"complete":True,"wall_ms":12.,
            "peak_resident_bytes":1024,"verified_views":200,"verification_ms_outside_page":1.,
            "reads":[{"index":i,"ticket":i+1,"outcome":"ready","queue_ms":0.,"owner_read_ms":.1,
                      "metrics":{"catalog_identity_ms":0.,"store_read_checksum_ms":0.,"header_decode_ms":.1,
                                 "total_ms":.1,"returned_pixels":True}} for i in range(200)]}


class NavigationContractTests(unittest.TestCase):
    def test_fixed_plan_and_nearest_rank_do_not_drop_tail(self):
        self.assertEqual(len(plan()),44)
        for profile in ("standard","constrained"):
            self.assertEqual(sum(p==profile and w=="fresh" for p,w,_ in plan()),20)
        self.assertEqual(len(expected_trials("warm")),104)
        self.assertEqual(len(expected_trials("navigation")),10)
        self.assertEqual(distribution([1]*99+[999])["p99"],1)
        self.assertEqual(distribution([1]*99+[999])["max"],999)
        for values in ([],[float("nan")],[float("inf")],[-1],[True]):
            with self.assertRaises(ValueError): distribution(values)

    def test_page_requires_full_unique_visible_identity_and_finite_owned_pixels(self):
        validate_trial(page(),"fresh",0)
        for field in ("duplicate_index","duplicate_ticket","resource_limit","missing","unowned","nonfinite","rss"):
            row=page()
            if field=="duplicate_index": row["reads"][0]["index"]=1
            elif field=="duplicate_ticket": row["reads"][0]["ticket"]=2
            elif field in ("resource_limit","missing"): row["reads"][0]["outcome"]=field
            elif field=="unowned": row["reads"][0]["metrics"]["returned_pixels"]=False
            elif field=="nonfinite": row["reads"][0]["owner_read_ms"]=float("nan")
            else: row["peak_resident_bytes"]=None
            with self.assertRaises(ValueError): validate_trial(row,"fresh",0)

    def test_navigation_detects_late_and_lost_consumers_and_shifted_clock(self):
        row=page(); row.update(kind="navigation",verification_ms_in_trace_wall=1.,
            viewports=[{"viewport":i,"scheduled_ms":i*50,"overrun_ms":0.} for i in range(100)],
            events=[{"action":"submit","ticket":i+1,"index":i} for i in range(200)])
        validate_trial(row,"navigation",0)
        for mode in ("late","lost","clock","wrongindex"):
            bad=copy.deepcopy(row)
            if mode=="late": bad["events"].append({"action":"cancel","ticket":1,"index":0})
            elif mode=="lost": bad["events"].append({"action":"submit","ticket":999,"index":999})
            elif mode=="clock": bad["viewports"][1]["scheduled_ms"]=60
            else: bad["reads"][0]["index"]=50
            with self.assertRaises(ValueError): validate_trial(bad,"navigation",0)

    def test_review_binding_requires_exact_whole_archive_binary_and_protocol(self):
        files={k:"a"*64 for k in ("archive","binary","worker","protocol","fixture","dataset","layout_receipt","storage","coordinator")}
        binding={"version":2,"catalog_schema":6,"clean":True,"source_revision":"b"*40,"planned_measured_children":44,"planned_verifiers":44,
                 **{name+"_sha256":value for name,value in files.items()}}
        validate_binding(binding,files)
        for field in ("catalog_schema","version","clean","archive_sha256","binary_sha256","protocol_sha256","planned_measured_children"):
            bad=copy.deepcopy(binding);bad[field]=None
            with self.assertRaises(ValueError): validate_binding(bad,files)


if __name__=="__main__": unittest.main()
