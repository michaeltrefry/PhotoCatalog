import unittest
from pathlib import Path
from edit_admission import validate_record_paths


class ReceiptPathAdmission(unittest.TestCase):
    def test_receipts_cannot_escape_or_alias_a_different_case(self):
        root=Path('/private/campaign')
        case={'id':'export-example','phase':'export'}
        record=dict(probe_output=str(root/'export-example-output'),
            request_path=str(root/'export-example-output/request.json'),
            verification_path=str(root/'verify-export-example-verification.json'),
            probe_supervisor_path=str(root/'export-example/result.json'),
            verify_supervisor_path=str(root/'verify-export-example/result.json'),
            cleanup_path=str(root/'export-example-cleanup.json'))
        validate_record_paths(record,root,case)
        for name in record:
            for value in ('/private/unrelated/result.json',str(root/'different-case/result.json'),None):
                with self.subTest(name=name,value=value),self.assertRaises(ValueError):
                    validate_record_paths(dict(record,**{name:value}),root,case)
        case['phase']='correctness'
        with self.assertRaises(ValueError):validate_record_paths(record,root,case)
        validate_record_paths(dict(record,cleanup_path=None),root,case)



class OuterFundingAdmission(unittest.TestCase):
    def test_actual_funding_requires_new_outer_and_host_allowances(self):
        import copy
        import edit_disk_budget as disk
        import edit_qualification as q
        from edit_admission import validate_funding
        manifest=dict(version=1,inputs=[dict(id=i,path='/owned/'+i,sha256='a'*64,width=512,height=512) for i in q.IDS])
        funding=disk.budget(manifest)
        binding=dict(funding=funding,outer_owner=funding['outer_owner'],preparation_owner=funding['preparation_owner'],
            **{k:funding[k] for k in ('retained_bound_bytes','active_bound_bytes','copies_bound_bytes','free_reserve_bytes','minimum_free_bytes')})
        validate_funding(binding,funding)
        for field in ('outer_owner','preparation_owner','minimum_free_bytes','funding'):
            broken=copy.deepcopy(binding);del broken[field]
            with self.assertRaises(ValueError):validate_funding(broken,funding)
        for path in (('deadline_seconds',),('supervision','max_seen'),('host_logs','max_bytes')):
            broken=copy.deepcopy(binding);current=broken['outer_owner']
            for key in path[:-1]:current=current[key]
            current[path[-1]]-=1
            with self.assertRaises(ValueError):validate_funding(broken,funding)
        broken=copy.deepcopy(binding)
        broken['preparation_owner']['host_logs']['max_bytes']-=1
        with self.assertRaises(ValueError):validate_funding(broken,funding)
        broken=copy.deepcopy(binding)
        broken['funding']['components']['outer_and_host']=0
        with self.assertRaises(ValueError):validate_funding(broken,funding)

if __name__=='__main__':unittest.main()
