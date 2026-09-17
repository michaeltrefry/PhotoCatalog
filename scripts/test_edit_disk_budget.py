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
        for volume in value['volumes'].values():
            peak=sum(volume[k] for k in ('retained_bound_bytes','active_bound_bytes','copies_bound_bytes','free_reserve_bytes'))
            self.assertGreaterEqual(volume['minimum_free_bytes'],peak)
            self.assertEqual(volume['minimum_free_bytes']%q.GIB,0)
        self.assertGreaterEqual(value['active_bound_bytes'],22*512*q.MIB)
        self.assertEqual(sum(v['retained_bound_bytes'] for v in value['volumes'].values()),value['retained_bound_bytes'])
        self.assertEqual(sum(v['active_bound_bytes'] for v in value['volumes'].values()),value['active_bound_bytes'])
        self.assertEqual(sum(v['copies_bound_bytes'] for v in value['volumes'].values()),value['copies_bound_bytes'])
        self.assertEqual(sum(v['output_stop_bytes'] for v in value['volumes'].values()),value['output_stop_bytes'])
        self.assertEqual(value['volumes']['service']['components']['active']['export_staging_extent'],512*q.MIB)
        self.assertEqual(value['minimum_free_bytes_by_volume']['service'],102*q.GIB)
        self.assertEqual(value['minimum_free_bytes_by_volume']['artifact'],value['volumes']['artifact']['minimum_free_bytes'])
        self.assertEqual(value['volumes']['artifact']['post_campaign_bytes'],64*q.MIB+64*1024)
        self.assertEqual(value['volumes']['artifact']['campaign_admission_bytes'],
                         value['volumes']['artifact']['minimum_free_bytes']+64*q.MIB+64*1024)
        self.assertEqual(value['volumes']['artifact']['full_sequence_initial_free_bytes'],
                         value['volumes']['artifact']['campaign_admission_bytes']+value['copies_bound_bytes'])

    def test_bigger_source_grows_funding_without_reducing_other_terms(self):
        small=manifest()
        large=manifest()
        large['inputs'][0].update(width=6000,height=5000)
        a,b=disk.budget(small),disk.budget(large)
        self.assertGreater(b['components']['raw'],a['components']['raw'])
        self.assertGreaterEqual(b['volumes']['artifact']['minimum_free_bytes'],a['volumes']['artifact']['minimum_free_bytes'])
        self.assertEqual(b['volumes']['service'],a['volumes']['service'])

    def test_outer_and_host_funding_is_additive_and_exact(self):
        value=disk.budget(manifest());outer=value['outer_owner']
        self.assertEqual(value['components']['outer_and_host'],16*q.GIB+137*q.MIB)
        self.assertEqual(value['components']['evidence_streams'],1074*(40*q.MIB+256*1024))
        self.assertEqual(outer['supervision']['max_seen'],131072)
        self.assertEqual(outer['host_logs'],dict(max_bytes=8*q.GIB,max_record_bytes=q.MIB))
        self.assertEqual(value['components']['preparation_outer_and_host'],657*q.MIB)
        preparation=value['preparation_owner']
        self.assertEqual(preparation['deadline_seconds'],3900)
        self.assertEqual(preparation['supervision'],dict(max_active=4,max_seen=8192,max_telemetry_bytes=128*q.MIB,
            max_sample_bytes=8192,max_identity_bytes=8*q.MIB,max_identity_event_bytes=512))
        self.assertEqual(preparation['host_logs'],dict(max_bytes=512*q.MIB,max_record_bytes=q.MIB))
        self.assertEqual(sum(value['components'].values()),value['retained_bound_bytes'])
        self.assertEqual(value['volumes']['service']['components']['retained']['allocation_overhead'],135*1024*4096)
        self.assertEqual(value['volumes']['artifact']['components']['retained']['allocation_overhead'],
                         value['components']['allocation_overhead']-135*1024*4096)
        for volume in value['volumes'].values():
            self.assertEqual(sum(volume['components']['retained'].values()),volume['retained_bound_bytes'])
            self.assertEqual(sum(volume['components']['active'].values()),volume['active_bound_bytes'])
            self.assertEqual(sum(volume['components']['copies'].values()),volume['copies_bound_bytes'])
            self.assertEqual(sum(volume['components']['post_campaign'].values()),volume['post_campaign_bytes'])

    def test_fixed_registry_deadlines_and_child_lifetimes(self):
        cases=disk.complete_cases(manifest())
        self.assertEqual(sum(c['deadline_seconds'] for c in cases),367800)
        phases={}
        for case in cases:phases.setdefault(case['phase'],[]).append(case)
        request_workers=sum(c['warmups']+c['repetitions'] for p in ('warm_service','first_raw','export','export_correctness') for c in phases[p])
        setups=sum(len(phases[p]) for p in ('warm_service','first_raw','export','export_correctness','overlap_import','overlap_export'))
        self.assertEqual((request_workers,setups),(11120,135))
        self.assertEqual(2*len(cases)+request_workers+setups+2+1+1+86401,98726)
        self.assertLess(864002*8192,8*q.GIB)
        self.assertEqual(2*131072*512,128*q.MIB)

if __name__=='__main__':
    unittest.main()
