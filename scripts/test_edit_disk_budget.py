import unittest
import edit_disk_budget as disk
import edit_qualification as q


def manifest():
    return dict(version=1,inputs=[dict(id=i,path='/owned/'+i,sha256='a'*64,width=512,height=512) for i in q.IDS])


class DiskContracts(unittest.TestCase):
    def test_all_cases_and_entire_peak_are_funded(self):
        value=disk.budget(manifest())
        self.assertEqual(value['proposed_probe_count'],533)
        self.assertEqual(value['proposed_total_children'],1074)
        peak=sum(value[k] for k in ('retained_bound_bytes','active_bound_bytes','copies_bound_bytes','free_reserve_bytes'))
        self.assertGreaterEqual(value['minimum_free_bytes'],peak)
        self.assertEqual(value['minimum_free_bytes']%q.GIB,0)
        self.assertGreaterEqual(value['active_bound_bytes'],22*512*q.MIB)

    def test_bigger_source_grows_funding_without_reducing_other_terms(self):
        small=manifest()
        large=manifest()
        large['inputs'][0].update(width=6000,height=5000)
        a,b=disk.budget(small),disk.budget(large)
        self.assertGreater(b['components']['raw'],a['components']['raw'])
        self.assertGreaterEqual(b['minimum_free_bytes'],a['minimum_free_bytes'])

if __name__=='__main__':
    unittest.main()
