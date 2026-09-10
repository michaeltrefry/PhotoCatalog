"""Source-contract tests, no workload launch or source-photo access."""
import copy
import unittest
from unittest.mock import patch
import edit_campaign as campaign
import edit_correctness_matrix as matrix


def binding():
    file=dict(path='/private/frozen',sha256='a'*64)
    return dict(version=2,pending_execution_gates=[],automatic_retries=0,
                actions=[dict(id='one',kind='probe',deadline_seconds=60,process_rss_bytes=100,group_rss_bytes=200)],
                **{key:file.copy() for key in ('python','probe','worker','verifier','fixture_generator',
                                              'source_archive','build_reference','protocol')},
                source_commit='b'*40,minimum_free_bytes=1000,retained_bound_bytes=300,
                active_bound_bytes=200,copies_bound_bytes=100,free_reserve_bytes=400)


class BindingContracts(unittest.TestCase):
    def check(self,value):
        with patch.object(campaign,'digest',return_value='a'*64):
            campaign.validate_binding(value)

    def test_funded_complete_binding_and_pending_rejection(self):
        self.check(binding())
        value=binding()
        value['pending_execution_gates']=['oracle incomplete']
        with self.assertRaises(ValueError): self.check(value)

    def test_minimum_funds_entire_peak(self):
        value=binding()
        value['minimum_free_bytes']=999
        with self.assertRaises(ValueError): self.check(value)

    def test_nonfinite_negative_and_oversize_deadlines_rejected(self):
        for bad in (float('nan'),float('inf'),-1,0,3601,True):
            value=binding()
            value['actions'][0]['deadline_seconds']=bad
            with self.assertRaises(ValueError): self.check(value)

    def test_windows_and_posix_action_path_escape_rejected(self):
        for bad in ('../escape','/tmp/foreign','C:\\foreign','two/parts',''):
            value=binding()
            value['actions'][0]['id']=bad
            with self.assertRaises(ValueError): self.check(value)

    def test_pid_reuse_is_not_live_old_identity(self):
        class Process:
            def create_time(self): return 20
            def status(self): return 'running'
        with patch.object(campaign.psutil,'Process',return_value=Process()):
            self.assertFalse(campaign.same_alive((42,10)))
            self.assertTrue(campaign.same_alive((42,20)))

    def test_matrix_has_all_profile_request_paths_and_durable_metadata(self):
        outputs=matrix.output_matrix()
        self.assertEqual(len(outputs),160)
        for name in ('jpeg8','png8','png16','tiff8','tiff16'):
            values=[c['outputs'][0]['profile']['kind'] for c in outputs if c['id'].startswith('format-'+name+'-')]
            self.assertEqual(set(values),{'srgb','linear_srgb','icc'})
        self.assertEqual(len(matrix.durable_metadata_cases()),6)
        packet=matrix.metadata(True)['xmp']
        for needed in ('rdf:about="urn:', 'q:qualified','q:structure','rdf:Bag','rdf:Seq','crs:Exposure2012','tiff:StripOffsets'):
            self.assertIn(needed,packet)

if __name__=='__main__':
    unittest.main()
