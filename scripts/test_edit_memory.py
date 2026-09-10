import copy
import unittest
import edit_memory


class ProcessMemoryContracts(unittest.TestCase):
    def request(self, phase):
        return dict(phase=phase, render={'max_live_bytes': 4096})

    def receipt(self):
        return dict(rss=dict(status='available', bytes=500))

    def metrics(self):
        return dict(pid=123, peak_resident_bytes=800,
                    peak_method='getrusage process high-water RSS; test', keys=[{'revision': 4}])

    def test_absent_boolean_or_over_limit_high_water_cannot_pass_sampled_memory(self):
        for invalid in (None, 0, True, 4097, float('nan')):
            receipt = self.receipt(); receipt['rss']['bytes'] = invalid
            with self.assertRaises(ValueError): edit_memory.evidence(self.request('kernel'), receipt, [])
        receipt = self.receipt(); receipt['rss']['status'] = 'unavailable'
        with self.assertRaises(ValueError): edit_memory.evidence(self.request('kernel'), receipt, [])

    def test_preview_peak_requires_actual_current_producer_and_available_method(self):
        value = dict(worker_metrics=self.metrics(), observed_worker_pids=[123], key={'revision': 4})
        result = edit_memory.evidence(self.request('warm_service'), self.receipt(), [value])
        self.assertEqual(result['maximum_process_peak_resident_bytes'], 800)
        for mutate in (lambda v: v.update(observed_worker_pids=[124]),
                       lambda v: v.update(key={'revision': 5}),
                       lambda v: v['worker_metrics'].update(peak_method='unavailable'),
                       lambda v: v['worker_metrics'].update(peak_resident_bytes=4097)):
            bad = copy.deepcopy(value); mutate(bad)
            with self.assertRaises(ValueError): edit_memory.evidence(self.request('warm_service'), self.receipt(), [bad])

    def test_export_peak_requires_matching_actual_started_event(self):
        value = dict(phases=dict(worker_pid=123, worker_peak_resident_bytes=800,
                                worker_peak_method=self.metrics()['peak_method']),
                     events=[{'event': {'state': 'started', 'pid': 123}}])
        edit_memory.evidence(self.request('export'), self.receipt(), [value])
        value['events'][0]['event']['pid'] = 124
        with self.assertRaises(ValueError): edit_memory.evidence(self.request('export'), self.receipt(), [value])

    def test_each_overlapping_worker_needs_a_completed_memory_receipt(self):
        values = [{'live_workers_before': [{'pid': 123}, {'pid': 124}]}]
        background = {'worker_metrics': [self.metrics()]}
        with self.assertRaises(ValueError): edit_memory.evidence(self.request('overlap_import'), self.receipt(), values, background=background)
        background['worker_metrics'].append({**self.metrics(), 'pid': 124, 'peak_resident_bytes': 900})
        result = edit_memory.evidence(self.request('overlap_import'), self.receipt(), values, background=background)
        self.assertEqual(result['maximum_worker_peak_resident_bytes'], 900)


if __name__ == '__main__': unittest.main()
