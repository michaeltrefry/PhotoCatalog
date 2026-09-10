"""Aggregate acceptance fixtures, no source photographs or native workloads."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import edit_aggregate as aggregate
import edit_disk_budget
import edit_qualification as q
import edit_statistics as stats
from test_edit_qualification import manifest
from test_edit_statistics import request, samples


class AggregateContracts(unittest.TestCase):
    def test_semantic_numbers_cannot_turn_boolean_into_configuration(self):
        self.assertTrue(aggregate.same({'kelvin': 4300}, {'kelvin': 4300.0}))
        for left, right in ((True, 1), (False, 0), ({'x': True}, {'x': 1}),
                            ([1], [1, 2]), (float('nan'), float('nan'))):
            self.assertFalse(aggregate.same(left, right))

    def test_whole_registry_rejects_missing_reordered_or_changed_case(self):
        cohort = manifest()
        cases = edit_disk_budget.complete_cases(cohort)
        binding = {'cases': cases, 'cases_sha256': hashlib.sha256(aggregate.canonical(cases)).hexdigest()}
        records = [{'id': case['id']} for case in cases]
        self.assertEqual(len(aggregate.registry(cohort, binding, records)), len(cases))
        for altered in (records[:-1], list(reversed(records)), records[:-1]+[records[0]]):
            with self.assertRaises(ValueError): aggregate.registry(cohort, binding, altered)
        bad = copy.deepcopy(binding)
        bad['cases'][0]['recipes'][0]['settings']['exposure_ev'] = 9.0
        bad['cases_sha256'] = hashlib.sha256(aggregate.canonical(bad['cases'])).hexdigest()
        with self.assertRaises(ValueError): aggregate.registry(cohort, bad, records)

    def case(self):
        r = request()
        r['source_blake3'] = 'b'*64
        values = samples(r)
        attempts = [dict(recipe_index=v['recipe_index'], iteration=v['iteration']) for v in values]
        receipt = {k: r[k] for k in ('phase', 'fixture_id', 'operation', 'source_sha256', 'source_blake3')}
        receipt.update(probe_complete=True, qualification_complete=False, error=None)
        verifier = dict(complete=True, error=None, result=dict(verified=True, whole_story_qualified=False,
            sample_count=len(values), coverage=sorted(aggregate.required_coverage(r))))
        return r, receipt, verifier, attempts, values

    def test_partial_or_overclaiming_verifier_cannot_award_case(self):
        r, receipt, verifier, attempts, values = self.case()
        aggregate.case_result(r, receipt, verifier, attempts, values)
        for mutate in (lambda p: p.update(complete=False),
                       lambda p: p['result'].update(whole_story_qualified=True),
                       lambda p: p['result'].update(remaining=['unverified pixels']),
                       lambda p: p['result'].update(coverage=['sample_identity', 'source_hashes']),
                       lambda p: p['result'].update(sample_count=101)):
            bad = copy.deepcopy(verifier); mutate(bad)
            with self.assertRaises(ValueError): aggregate.case_result(r, receipt, bad, attempts, values)

    def test_slow_valid_case_keeps_failure_and_tail_in_distribution(self):
        r, receipt, verifier, attempts, values = self.case()
        for value in values[-6:]: value['elapsed_ms'] = 1234
        result = aggregate.case_result(r, receipt, verifier, attempts, values)
        self.assertFalse(result['configurations'][0]['numeric_target_met'])
        self.assertEqual(result['configurations'][0]['elapsed']['p95_ms'], 1234)
        self.assertFalse(result['whole_story_qualified'])

    def repeats(self):
        results = []
        for fixture in q.IDS:
            for operation in ('all-0', 'all-1'):
                r = request('correctness')
                r.update(fixture_id=fixture, operation=operation)
                value = samples(r)[0]
                value['pixels'] = dict(width=1, height=1, rgba_f32le_blake3='c'*64,
                    minimum=[0, 0, 0, 1], maximum=[0, 0, 0, 1], alpha_zero_partial_opaque=[0, 0, 1], nonfinite=0)
                results.append((r, [value]))
        return results

    def test_repeated_pixel_changes_or_missing_camera_are_not_pooled_away(self):
        values = self.repeats()
        self.assertEqual(len(aggregate.deterministic_pairs(values)), 30)
        with self.assertRaises(ValueError): aggregate.deterministic_pairs(values[:-1])
        values[-1][1][0]['pixels']['rgba_f32le_blake3'] = 'd'*64
        with self.assertRaisesRegex(ValueError, 'repeated pixel observations differ'):
            aggregate.deterministic_pairs(values)

    def supervisor_fixture(self, root):
        folder = root/'action'; folder.mkdir()
        limits = dict(deadline_seconds=10, process_rss_bytes=100, group_rss_bytes=200, free_reserve_bytes=50)
        command = ['/fixed/probe', '--request', '/fixed/request']
        def write(name, value): (folder/name).write_text(json.dumps(value)+'\n')
        write('start.json', dict(command=command, limits=limits, started={'monotonic_ns': 100}))
        write('spawn.json', dict(pid=123, create_time=100.25))
        telemetry = dict(at={'monotonic_ns': 150}, processes=[dict(pid=123, create_time=100.25, rss=80, status='running')], total_rss=80, free_bytes=100)
        write('processes.jsonl', telemetry)
        result = dict(complete=True, error=None, samples=1, sampled_peak_group_rss=80,
                      finished={'monotonic_ns': 200}, captures={},
                      ownership=dict(known_absent=True, root_reaped=True, root_returncode=0, remaining=[], errors=[]))
        for name in ('stdout.log', 'stderr.log', 'processes.jsonl'):
            if name.endswith('.log'): (folder/name).write_bytes(b'')
            data = (folder/name).read_bytes()
            result[name] = dict(bytes=len(data), sha256=hashlib.sha256(data).hexdigest())
            if name.endswith('.log'):
                result['captures'][name] = dict(reader_joined=True, eof=True, truncated=False, errors=[],
                    limit_bytes=4*aggregate.MIB, observed_bytes=len(data), retained_bytes=len(data))
        write('result.json', result)
        return folder, result, command, limits

    def test_supervisor_launch_cleanup_pid_and_stream_hash_are_required(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            folder, result, command, limits = self.supervisor_fixture(root)
            aggregate.supervisor(root, folder, str(folder/'result.json'), command, limits, 123)
            with self.assertRaises(ValueError): aggregate.supervisor(root, folder, str(folder/'result.json'), command, limits, 124)
            for mutate in (lambda r: r['ownership'].update(root_reaped=False),
                           lambda r: r['ownership'].update(root_returncode=True),
                           lambda r: r.update(samples=2),
                           lambda r: r.update(sampled_peak_group_rss=79),
                           lambda r: r['captures']['stdout.log'].update(eof=False)):
                bad = copy.deepcopy(result); mutate(bad)
                (folder/'result.json').write_text(json.dumps(bad))
                with self.assertRaises(ValueError): aggregate.supervisor(root, folder, str(folder/'result.json'), command, limits)
            (folder/'result.json').write_text(json.dumps(result))
            (folder/'stdout.log').write_bytes(b'unbound output')
            with self.assertRaises(ValueError): aggregate.supervisor(root, folder, str(folder/'result.json'), command, limits)

    def test_owned_artifact_symlink_and_path_escape_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            owned = root/'owned'; owned.mkdir()
            outside = root/'outside'; outside.write_text('private')
            alias = owned/'alias'; alias.symlink_to(outside)
            for path in (outside, alias):
                with self.assertRaises(ValueError): aggregate.admitted_file(owned, path, 100)

    def test_metadata_resolution_and_resource_requests_cannot_silently_change(self):
        import edit_correctness_matrix as matrix
        normal = q.plan(manifest())['normal_limits']
        cases = (matrix.output_matrix()[0], matrix.durable_metadata_cases()[0], matrix.support_matrix()[0])
        for case in cases:
            with self.subTest(case=case['id']):
                r = {k: copy.deepcopy(case[k]) for k in ('phase', 'fixture_id', 'operation', 'recipes', 'outputs', 'warmups', 'repetitions')}
                limits = case.get('limits', {})
                r.update(metadata=aggregate.normalized_metadata(case.get('metadata', {})),
                         resolve_embedded=case.get('resolve_embedded', False),
                         decode=copy.deepcopy(limits.get('decode', normal['decode'])),
                         render=copy.deepcopy(limits.get('render', normal['render'])),
                         encoded_extent=limits.get('encoded_extent', case.get('encoded_extent', normal['encoded_extent'])))
                aggregate.case_semantics(case, r, normal)
                changes = [lambda v: v.update(resolve_embedded=not v['resolve_embedded']),
                           lambda v: v['decode'].update(max_allocation_bytes=1),
                           lambda v: v['render'].update(max_live_bytes=1),
                           lambda v: v.update(encoded_extent=v['encoded_extent']+1)]
                if case.get('metadata'):
                    changes.append(lambda v: v.update(metadata={}))
                for mutate in changes:
                    changed = copy.deepcopy(r); mutate(changed)
                    with self.assertRaises(ValueError): aggregate.case_semantics(case, changed, normal)

    def test_empty_foreign_or_out_of_interval_telemetry_is_unavailable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            folder, original, command, limits = self.supervisor_fixture(root)
            for change in ('empty', 'foreign', 'old', 'late'):
                result = copy.deepcopy(original)
                row = dict(at={'monotonic_ns': 150}, processes=[dict(pid=123, create_time=100.25, rss=80, status='running')], total_rss=80, free_bytes=100)
                if change == 'empty':
                    row.update(processes=[], total_rss=0); result['sampled_peak_group_rss'] = 0
                elif change == 'foreign': row['processes'][0]['create_time'] = 100.5
                elif change == 'old': row['at']['monotonic_ns'] = 99
                else: row['at']['monotonic_ns'] = 201
                data = (json.dumps(row)+'\n').encode()
                (folder/'processes.jsonl').write_bytes(data)
                result['processes.jsonl'] = dict(bytes=len(data), sha256=hashlib.sha256(data).hexdigest())
                (folder/'result.json').write_text(json.dumps(result))
                with self.assertRaises(ValueError): aggregate.supervisor(root, folder, str(folder/'result.json'), command, limits, 123)

    def test_generated_preparation_cannot_enter_measured_roster_or_be_omitted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            for records in ([], [{'id': 'unknown'}], [{'id': key} for key in aggregate.edit_fixtures.FIXTURES][:-1]):
                with self.assertRaises(ValueError): aggregate.preparation(root, {'preparation_records': records})

    def test_export_cleanup_requires_all_readbacks_retained_hash_and_disposable_roots(self):
        from blake3 import blake3
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            output = root/'case-output'; output.mkdir()
            r = dict(phase='export', warmups=2, encoded_extent=1024, output=str(output))
            record = dict(id='case', cleanup_path=str(root/'case-cleanup.json'),
                verification_path=str(root/'verification.json'),
                probe_supervisor_path=str(root/'probe.json'), verify_supervisor_path=str(root/'verify.json'))
            rows, encoded, files, directories = [], [], [], [str(output/'catalog')]
            for iteration in range(22):
                data = ('encoded '+str(iteration)).encode()
                path = str(output/f'export-{iteration}.image')
                sha = hashlib.sha256(data).hexdigest()
                recovery = str(output/f'.photocatalog-photo-export-operation-{iteration}')
                rows.append(dict(iteration=iteration, path=path, blake3=blake3(data).hexdigest(),
                                 items=[dict(receipt={'recovery_directory': recovery})]))
                encoded.append(dict(path=path, sha256=sha))
                directories.append(recovery)
                if iteration == 2: Path(path).write_bytes(data)
                else: files.append(dict(path=path, bytes=len(data), sha256=sha))
            proof = {'result': {'encoded': encoded}}
            for name, value in (('verification.json', proof), ('probe.json', {}), ('verify.json', {})):
                (root/name).write_text(json.dumps(value))
            retained = dict(path=rows[2]['path'], sha256=encoded[2]['sha256'], blake3=rows[2]['blake3'])
            start = dict(case_id='case', retained=retained, delete_files=files, delete_directories=directories)
            for key, value in (('verifier_receipt_sha256', 'verification.json'),
                               ('probe_supervisor_sha256', 'probe.json'), ('verify_supervisor_sha256', 'verify.json')):
                start[key] = hashlib.sha256((root/value).read_bytes()).hexdigest()
            def retain_receipts(plan):
                data = json.dumps(plan).encode()
                (root/'case-cleanup-start.json').write_bytes(data)
                done = dict(complete=True, error=None, retained=retained, start_sha256=hashlib.sha256(data).hexdigest(),
                            deleted_paths=[f['path'] for f in plan['delete_files']]+plan['delete_directories'])
                (root/'case-cleanup.json').write_text(json.dumps(done))
            retain_receipts(start)
            result = aggregate.cleanup_evidence(root, record, r, proof, rows)
            self.assertEqual(result['deleted_files'], 21)
            for mutate in (lambda p: p['delete_directories'].pop(),
                           lambda p: p['delete_files'][0].update(sha256='f'*64),
                           lambda p: p['delete_files'].append({'path': str(root/'unrelated'), 'sha256': 'a'*64})):
                changed = copy.deepcopy(start); mutate(changed); retain_receipts(changed)
                with self.assertRaises(ValueError): aggregate.cleanup_evidence(root, record, r, proof, rows)
            retain_receipts(start)
            Path(retained['path']).write_bytes(b'changed retained output')
            with self.assertRaises(ValueError): aggregate.cleanup_evidence(root, record, r, proof, rows)


if __name__ == '__main__':
    unittest.main()
