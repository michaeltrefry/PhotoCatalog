"""Evidence admission regressions; no image processing or workload execution."""
import copy
import io
import unittest
import edit_verify as verify


class VerificationAdmission(unittest.TestCase):
    def setUp(self):
        self.request=dict(phase='correctness',warmups=0,repetitions=1,
                          recipes=[{},{}],outputs=[{},{}])
        self.attempts=[dict(recipe_index=i,iteration=0) for i in range(2)]
        self.values=copy.deepcopy(self.attempts)

    def test_exact_cartesian_attempt_and_observation_coverage(self):
        verify.sample_coverage(self.request,self.attempts,self.values)
        for bad in (999,-1,True):
            values=copy.deepcopy(self.values)
            values[1]['recipe_index']=bad
            with self.assertRaises(ValueError):
                verify.sample_coverage(self.request,self.attempts,values)

    def test_attempts_cannot_be_duplicated_or_detached_from_observations(self):
        for attempts in ([self.attempts[0]]*2,list(reversed(self.attempts)),self.attempts[:1]):
            with self.assertRaises(ValueError):
                verify.sample_coverage(self.request,attempts,self.values)

    def test_large_cancellation_requires_same_explicit_recipe_identity(self):
        request=dict(phase='large_cancellation',warmups=0,repetitions=1,
                     recipes=[{}],outputs=[])
        sample=dict(recipe_index=0,iteration=0)
        verify.sample_coverage(request,[sample],[sample])
        with self.assertRaises(ValueError):
            verify.sample_coverage(request,[dict(iteration=0)],[sample])
        with self.assertRaises(ValueError):
            verify.sample_coverage(request,[sample],[dict(recipe_index=1,iteration=0)])

    def test_missing_all_or_one_encoding_and_duplicate_paths_rejected(self):
        for value in ({},{'exports':[]},{'exports':[{'path':'one'}]},
                      {'exports':[{'path':'one'},{'path':'one'}]}):
            with self.assertRaises(ValueError):
                list(verify.output_coverage(self.request,value))
        self.assertEqual(len(list(verify.output_coverage(self.request,
                         {'exports':[{'path':'one'},{'path':'two'}]}))),2)

    def test_bounded_read_rejects_growing_total_and_single_long_line(self):
        class Bounded(io.BytesIO):
            def readline(self,size=-1):
                if size<0 or size>17:
                    raise AssertionError('unbounded line read')
                return super().readline(size)
        with self.assertRaises(ValueError):
            list(verify.sample_records(Bounded(b'{}\n'*20),total_limit=16,line_limit=16))
        with self.assertRaises(ValueError):
            list(verify.sample_records(Bounded(b' '*40+b'\n'),total_limit=100,line_limit=16))

    def test_nonfinite_and_duplicate_json_fields_rejected(self):
        for data in (b'{"n":NaN}',b'{"n":Infinity}',b'{"n":1e9999}',b'{"n":1,"n":2}'):
            with self.assertRaises(ValueError): verify.strict_json(data)

    def test_stale_actor_pid_and_reused_kernel_identity_cannot_prove_overlap(self):
        identity=dict(pid=42,parent_pid=10,start_seconds=1,start_microseconds=0)
        value=dict(live_workers_before=[identity],live_workers_after=[identity.copy()],
                   owned_pids_before=[42],owned_pids_after=[42],
                   **{name:dict(unix_ns=str(2_000_000_000+i)) for i,name in enumerate(('live_before_at','started','finished','live_after_at'))})
        receipt=dict(probe_pid=10)
        telemetry=[dict(at=dict(unix_ns=2_000_000_000),processes=[dict(pid=42,create_time=1.,status='running')])]
        verify.overlap_proof(value,receipt,telemetry)
        with self.assertRaises(ValueError):verify.overlap_proof(value,receipt,[])
        telemetry[0]['processes'][0]['create_time']=.5
        with self.assertRaises(ValueError):verify.overlap_proof(value,receipt,telemetry)
        value['live_workers_after'][0]['start_microseconds']=1
        with self.assertRaises(ValueError):verify.overlap_proof(value,receipt,[])


if __name__=='__main__':
    unittest.main()
