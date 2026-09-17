import copy
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock

from evaluate_export_overlap_v2 import evaluate, CLOCK
from observe_export_native_v2 import lifecycle, same_birth, psutil
import observe_export_native_v2 as observer_module
from measurement_clock_conformance import diagnose, read_bounded_line


def fixture():
    samples, events, anchors = [], [], []
    for index in range(201):
        anchors.append({'anchor_id': index + 1, 'send_event': index * 4 + 1, 'receive_event': index * 4 + 2, 'error': None,
                        'native': {'anchor_id': index + 1, 'run_id': 'run', 'session_id': 'session', 'native_pid': 42, 'clock': CLOCK, 'monotonic_ns': str(1000 + index * 1000)}})
        if index < 200:
            samples.append({'ordinal': index + 1, 'kind': 'edit', 'outcome': 'complete', 'during_export': True, 'durable_us': 100000, 'presentation_us': 110000})
            events.append({'ordinal': index + 1, 'start_event': index * 4 + 3, 'end_event': index * 4 + 4})
    receipt = {'protocol': 2, 'run_id': 'run', 'time_origin_ms': 123, 'overflowed': 0, 'samples': samples,
               'clock_alignment': {'model': 'causal_native_brackets_v1', 'interval_ms': 100, 'duration_ms': 300000, 'stop_reason': 'finalized', 'anchors': anchors, 'sample_events': events}}
    stage = {'job': 'job', 'sequence': 1, 'attempt': 'attempt', 'authority': 'authority', 'active_lock_device_inode': [1, 2]}
    segment = {'segment': 1, 'pid': 43, 'birth_unix_s': 123, 'stage': stage, 'first_positive_after_monotonic_ns': 1000, 'last_positive_before_monotonic_ns': 201000, 'positive_observations': 2, 'lsof_confirmations': 1}
    observer = [{'kind': 'identity', 'protocol': 2, 'clock': CLOCK, 'job': 'job', 'root_pid': 42, 'executable_sha256': 'hash', 'monotonic_ns': 0},
                {'kind': 'summary', 'protocol': 2, 'clock': CLOCK, 'fatal_errors': 0, 'root_same_birth': True, 'executable_unchanged': True, 'segments': [segment], 'measurement_end_monotonic_ns': 202000}]
    declaration = {'run_id': 'run', 'job': 'job', 'executable_sha256': 'hash', 'input_ordinals': list(range(1, 201)), 'setup_cutoff_ordinal': 0}
    raw_proof(observer)
    return receipt, observer, declaration


def raw_proof(observer):
    identity, summary = observer[0], observer[-1]
    proof = []
    for segment in summary['segments']:
        for kind, before, after in [('stage_admitted', segment['first_positive_after_monotonic_ns'] - 1, segment['first_positive_after_monotonic_ns']),
                                    ('active', segment['last_positive_before_monotonic_ns'], segment['last_positive_before_monotonic_ns'] + 1)]:
            proof.append({'kind': kind, 'pid': segment['pid'], 'birth_unix_s': segment['birth_unix_s'], 'segment': segment['segment'],
                          'positive_before_monotonic_ns': before, 'positive_after_monotonic_ns': after, **segment['stage']})
    for index, segment in enumerate(summary['segments']):
        reason = 'child_not_seen' if index + 1 < len(summary['segments']) else 'observer_end'
        closed_ns = segment['last_positive_before_monotonic_ns'] + 2 if reason == 'child_not_seen' else summary['measurement_end_monotonic_ns']
        segment.update(closed_reason=reason, closed_observed_monotonic_ns=closed_ns)
        proof.append({'kind': 'segment_closed', 'segment': segment['segment'], 'pid': segment['pid'],
                      'birth_unix_s': segment['birth_unix_s'], 'closed_reason': reason, 'monotonic_ns': closed_ns})
        if reason == 'child_not_seen':
            proof.append({'kind': 'stage_not_seen', 'pid': segment['pid'], 'birth_unix_s': segment['birth_unix_s'], 'monotonic_ns': closed_ns})
    proof.sort(key=lambda row: row.get('positive_after_monotonic_ns', row.get('monotonic_ns')))
    summary.update(positive_observations=sum(row['kind'] in ('active', 'stage_admitted') for row in proof),
                   admitted_stages=len(summary['segments']), usable_segments=len(summary['segments']), observation_gaps=0)
    observer[:] = [identity, *proof, summary]


class EvaluationTests(unittest.TestCase):
    def test_raw_summary_bijection_and_aggregate_mutations_reject(self):
        a = fixture(); extra = copy.deepcopy(a[1][1]); extra['segment'] = 99
        a[1].insert(-1, extra)
        with self.assertRaisesRegex(ValueError, 'bijection'): evaluate(*a)
        a = fixture(); a[1][-1]['segments'] = []
        with self.assertRaisesRegex(ValueError, 'bijection'): evaluate(*a)
        a = fixture(); a[1].insert(-1, copy.deepcopy(a[1][2]))
        with self.assertRaisesRegex(ValueError, 'count'): evaluate(*a)
        for field in ('positive_observations', 'admitted_stages', 'usable_segments', 'observation_gaps'):
            a = fixture(); a[1][-1][field] += 1
            with self.assertRaisesRegex(ValueError, 'count'): evaluate(*a)

    def test_closure_requires_raw_boundary_reason_and_end_evidence(self):
        a = fixture(); a[1].pop(-2)
        with self.assertRaisesRegex(ValueError, 'closure bijection'): evaluate(*a)
        a = fixture(); a[1][-1]['segments'][0]['closed_observed_monotonic_ns'] -= 1
        with self.assertRaisesRegex(ValueError, 'closure boundary'): evaluate(*a)
        a = fixture(); a[1][-1]['segments'][0]['closed_reason'] = 'denied_access'
        a[1][-2]['closed_reason'] = 'denied_access'
        with self.assertRaisesRegex(ValueError, 'closure cause'): evaluate(*a)
        a = fixture(); a[1][-1]['segments'][0]['closed_observed_monotonic_ns'] -= 1; a[1][-2]['monotonic_ns'] -= 1
        with self.assertRaisesRegex(ValueError, 'end closure'): evaluate(*a)

    def test_extra_postsetup_input_and_noncontiguous_declaration_rejected(self):
        a = fixture(); a[0]['samples'].append({**a[0]['samples'][-1], 'ordinal': 201})
        with self.assertRaisesRegex(ValueError, 'undeclared'): evaluate(*a)
        a = fixture(); a[2]['input_ordinals'][-1] = 201
        with self.assertRaisesRegex(ValueError, 'contiguous'): evaluate(*a)

    def test_raw_gap_and_summary_mismatch_rejected(self):
        a = fixture(); a[1].insert(2, {'kind': 'observation_gap', 'pid': 43, 'monotonic_ns': 1500})
        a[1][-1]['observation_gaps'] = 1
        with self.assertRaisesRegex(ValueError, 'gap'): evaluate(*a)
        a = fixture(); a[1][-1]['segments'][0]['last_positive_before_monotonic_ns'] -= 1
        with self.assertRaisesRegex(ValueError, 'endpoint'): evaluate(*a)

    def test_exact_boundary_and_budget(self):
        r = evaluate(*fixture())
        self.assertEqual(r['cohort'], list(range(1, 101)))
        self.assertEqual(r['durable_p95_ms'], 100)
        self.assertEqual(r['verdict'], 'TIMING_PASS_REQUIRES_EXPORT_RECONCILIATION')

    def test_wall_epoch_and_latency_cannot_change_selection(self):
        a = fixture(); original = evaluate(*a)
        a[0]['time_origin_ms'] = -99999999
        a[1][0]['wall_ns'] = 10**30
        a[1][-1]['max_clock_offset_drift_ns'] = 10**30
        for index, sample in enumerate(a[0]['samples']):
            sample['durable_us'] = 109000 if index % 2 else 0
        changed = evaluate(*a)
        self.assertEqual(original['cohort'], changed['cohort'])
        self.assertEqual(changed['verdict'], 'FAILED_LATENCY')

    def test_missing_and_failed_inputs_never_pass(self):
        a = fixture(); a[0]['samples'].pop()
        with self.assertRaisesRegex(ValueError, 'Missing'):
            evaluate(*a)
        a = fixture(); a[0]['samples'][150]['outcome'] = 'backend_error'
        self.assertEqual(evaluate(*a)['verdict'], 'FAILED_ACTIVE_INPUT')

    def test_gap_cannot_be_bridged_or_reopened(self):
        a = fixture(); first = a[1][-1]['segments'][0]; first['last_positive_before_monotonic_ns'] = 100000
        second = copy.deepcopy(first); second.update(segment=2, pid=44, first_positive_after_monotonic_ns=151000, last_positive_before_monotonic_ns=201000)
        a[1][-1]['segments'].append(second)
        raw_proof(a[1])
        result = evaluate(*a)
        self.assertNotIn(100, result['cohort']); self.assertEqual(result['cohort'][99], 151)
        second['pid'] = 43
        with self.assertRaisesRegex(ValueError, 'Reopened'):
            evaluate(*a)

    def test_delayed_ipc_cannot_imply_tighter_envelope(self):
        a = fixture()
        a[1][-1]['segments'][0]['first_positive_after_monotonic_ns'] = 1500
        raw_proof(a[1])
        self.assertEqual(evaluate(*a)['cohort'][0], 2)
        # A reply received after sample start cannot be used as its lower bound.
        a = fixture(); a[0]['clock_alignment']['anchors'][0]['receive_event'] = 4
        a[0]['clock_alignment']['sample_events'][0]['end_event'] = 3
        with self.assertRaises(ValueError): evaluate(*a)

    def test_wrong_identity_unknown_failure_and_observer_errors(self):
        for field, bad in [('session_id', 'different'), ('native_pid', 88), ('clock', 'wall'), ('run_id', 'other')]:
            a = fixture(); a[0]['clock_alignment']['anchors'][1]['native'][field] = bad
            with self.assertRaises(ValueError): evaluate(*a)
        a = fixture(); a[1][-1]['segments'][0]['first_positive_after_monotonic_ns'] = 1500
        raw_proof(a[1])
        a[0]['samples'][0]['outcome'] = 'backend_error'
        self.assertEqual(evaluate(*a)['verdict'], 'PARTIAL')
        a = fixture(); a[1][-1]['fatal_errors'] = 1
        with self.assertRaisesRegex(ValueError, 'Observer errors'): evaluate(*a)


class LifecycleTests(unittest.TestCase):
    def test_denial_rechecks_live_then_gone_preserve_all_observations(self):
        evidence = self.rechecks([{'state': 'live', 'detail': 'same_birth', 'observed_status': 'running', 'observed_birth_unix_s': 12},
                                  {'state': 'gone', 'detail': 'zombie', 'observed_status': psutil.STATUS_ZOMBIE, 'observed_birth_unix_s': None}])
        self.assertEqual(evidence['lifecycle'], 'gone')
        self.assertEqual([row['state'] for row in evidence['lifecycle_probes']], ['live', 'gone'])
        self.assertTrue(all(row['within_budget'] for row in evidence['lifecycle_probes']))

    def rechecks(self, observations, read_ns=0, window_ns=250_000_000):
        now = 1_000_000_000
        index = 0
        def inspect(*_):
            nonlocal index, now
            result = observations[min(index, len(observations) - 1)]
            index += 1; now += read_ns
            return result
        def sleep(seconds):
            nonlocal now
            now += round(seconds * 1e9)
        with patch.object(observer_module, 'lifecycle_observation', side_effect=inspect), \
             patch.object(observer_module.time, 'monotonic_ns', side_effect=lambda: now), \
             patch.object(observer_module.time, 'time_ns', side_effect=lambda: 10_000_000_000 + now), \
             patch.object(observer_module.time, 'sleep', side_effect=sleep):
            evidence = observer_module.recheck_denied_lifecycle(42, 12, 1_000_000_000, 1_000_000_000 + window_ns)
        self.assertLessEqual(len(evidence['lifecycle_probes']), 11)
        return evidence

    def test_denial_persistent_live_unknown_reuse_and_deadline_fail_closed(self):
        for state in ('live', 'unknown'):
            evidence = self.rechecks([{'state': state, 'detail': 'same_birth' if state == 'live' else 'AccessDenied'}])
            self.assertEqual(evidence['lifecycle'], state)
            self.assertGreater(len(evidence['lifecycle_probes']), 1)
        evidence = self.rechecks([{'state': 'gone', 'detail': 'pid_reused', 'observed_birth_unix_s': 13}])
        self.assertEqual((evidence['lifecycle'], evidence['lifecycle_detail']), ('gone', 'pid_reused'))
        late = self.rechecks([{'state': 'gone', 'detail': 'zombie'}], read_ns=250_000_001)
        self.assertEqual(late['lifecycle'], 'unknown')
        self.assertFalse(late['lifecycle_probes'][0]['within_budget'])
        self.assertEqual(len(late['lifecycle_probes']), 1)
        expired = self.rechecks([{'state': 'gone', 'detail': 'zombie'}], window_ns=0)
        self.assertEqual(expired['lifecycle'], 'unknown'); self.assertEqual(expired['lifecycle_probes'], [])

    def test_denial_exception_chain_retains_kernel_errno(self):
        try:
            try: raise PermissionError(1, 'kernel denied argv')
            except PermissionError as cause: raise psutil.AccessDenied(42) from cause
        except psutil.AccessDenied as error:
            evidence = observer_module.exception_evidence(error)
        self.assertEqual([row['type'] for row in evidence], ['AccessDenied', 'PermissionError'])
        self.assertEqual(evidence[1]['errno'], 1)

    @patch('observe_export_native_v2.psutil.Process')
    def test_live_gone_zombie_reuse_and_unknown(self, process):
        child = Mock(); process.return_value = child
        child.status.return_value = 'running'; child.create_time.return_value = 12
        self.assertEqual(lifecycle(42, 12)[0], 'live')
        self.assertEqual(lifecycle(42, 11), ('gone', 'pid_reused'))
        child.status.return_value = psutil.STATUS_ZOMBIE
        self.assertEqual(lifecycle(42, 12), ('gone', 'zombie'))
        process.side_effect = psutil.NoSuchProcess(42)
        self.assertEqual(lifecycle(42, 12)[0], 'gone')
        process.side_effect = psutil.AccessDenied(42)
        self.assertEqual(lifecycle(42, 12)[0], 'unknown')
        with self.assertRaises(psutil.AccessDenied): same_birth(42, 12)

    def run_observer(self, failure, operation='cmdline', gone=False, root_error=False):
        with tempfile.TemporaryDirectory() as temporary, ExitStack() as stack:
            base = Path(temporary); exe = base / 'installed'; exe.write_bytes(b'fixture')
            workers = base / 'export-workers'; workers.mkdir(); stage = workers / 'photo-worker-test'; stage.mkdir()
            output = base / 'observer.jsonl'
            root, child = Mock(pid=41), Mock(pid=42)
            root.create_time.return_value = 1; root.status.return_value = 'running'; root.exe.return_value = str(exe)
            child.create_time.return_value = 2; child.exe.return_value = str(exe); child.cwd.return_value = str(stage); child.ppid.return_value = 41
            ended = False
            calls = 0
            def operation_call():
                nonlocal calls, ended
                calls += 1
                if calls == 3:
                    ended = True
                    raise failure
                return [str(exe), observer_module.ROLE] if operation == 'cmdline' else str(stage)
            child.cmdline.return_value = [str(exe), observer_module.ROLE]
            getattr(child, operation).side_effect = operation_call
            post_denial_reads = 0
            def status():
                nonlocal post_denial_reads
                if ended:
                    post_denial_reads += 1
                return psutil.STATUS_ZOMBIE if ended and (gone is True or gone == 'later' and post_denial_reads >= 2) else 'running'
            child.status.side_effect = status
            child.is_running.return_value = True
            root.children.side_effect = [list([child]), list([child]), failure] if root_error else None
            root.children.return_value = [child]
            args = ['observer', '--pid', '41', '--exe', str(exe), '--exe-sha256', hashlib.sha256(b'fixture').hexdigest(),
                    '--export-workers', str(workers), '--job', 'job', '--output', str(output), '--seconds', '.05']
            bindings = {
                'sys.argv': args, 'sys.platform': 'darwin',
                'psutil.Process': lambda pid: root if pid == 41 else child,
                'time.monotonic': Mock(side_effect=[0, 0, .01, .02, .03, .06]),
                'time.monotonic_ns': Mock(side_effect=iter(range(1_000_000_000, 2_000_000_000, 1000))),
                'time.sleep': Mock(), 'time.get_clock_info': Mock(return_value=Mock(implementation='mach_absolute_time()')),
                'read_stage': Mock(return_value={'job': 'job', 'sequence': 1, 'attempt': 'attempt', 'authority': 'authority',
                    'active_lock': str(stage / 'active.lock'), 'active_lock_device_inode': [1, 2]}),
                'lock_contended': Mock(return_value=True),
                'child_holds_lock': Mock(return_value={'exact_path_open': True, 'elapsed_ns': 1}),
            }
            for name, value in bindings.items(): stack.enter_context(patch('observe_export_native_v2.' + name, value))
            real_recheck = observer_module.recheck_denied_lifecycle
            def recheck(*args):
                current = [json.loads(line) for line in output.read_text().splitlines()]
                self.assertEqual(current[-1]['kind'], 'segment_closed')
                self.assertEqual(current[-1]['closed_reason'], 'denied_access')
                self.assertEqual(current[-1]['monotonic_ns'], args[2])
                return real_recheck(*args)
            with patch.object(observer_module, 'recheck_denied_lifecycle', side_effect=recheck):
                code = observer_module.main()
            rows = [json.loads(line) for line in output.read_text().splitlines()]
            return code, rows

    def test_main_denied_exit_closes_without_reopening(self):
        code, rows = self.run_observer(psutil.AccessDenied(42), gone=True)
        self.assertEqual(code, 0)
        self.assertEqual(rows[-1]['fatal_errors'], 0)
        self.assertEqual(len(rows[-1]['segments']), 1)
        self.assertEqual(rows[-1]['segments'][0]['positive_observations'], 2)
        gap = next(row for row in rows if row['kind'] == 'observation_gap')
        self.assertEqual((gap['operation'], gap['lifecycle']), ('cmdline', 'gone'))

    def test_main_initially_live_then_gone_is_gap_without_late_positive(self):
        code, rows = self.run_observer(psutil.AccessDenied(42), gone='later')
        self.assertEqual(code, 0)
        gap = next(row for row in rows if row['kind'] == 'observation_gap')
        self.assertEqual([probe['state'] for probe in gap['lifecycle_probes']], ['live', 'gone'])
        closed = next(row for row in rows if row['kind'] == 'segment_closed')
        self.assertEqual(gap['denial_monotonic_ns'], closed['monotonic_ns'])
        self.assertLessEqual(closed['monotonic_ns'], gap['lifecycle_probes'][0]['before_monotonic_ns'])
        self.assertEqual(rows[-1]['segments'][0]['positive_observations'], 2)

    def test_main_live_denial_and_root_enumeration_remain_fatal(self):
        for options in ({}, {'operation': 'cwd'}, {'root_error': True}):
            code, rows = self.run_observer(psutil.AccessDenied(42), **options)
            self.assertEqual(code, 2); self.assertEqual(rows[-1]['fatal_errors'], 1)

    def test_main_disappearing_worker_is_gap(self):
        code, rows = self.run_observer(psutil.NoSuchProcess(42), operation='cwd')
        self.assertEqual(code, 0); self.assertEqual(rows[-1]['observation_gaps'], 1)


class ConformanceTests(unittest.TestCase):
    def test_diagnose_retains_partial_stdout_on_timeout_eof_overflow_and_bad_json(self):
        from types import SimpleNamespace
        cases = [('timeout', b'{"partial":', False),
                 ('closed before response', b'partial', True),
                 ('exceeds bound', b'x' * 4096, False),
                 ('Invalid diagnostic JSON', b'{oops}\n', False)]
        for message, partial, close_writer in cases:
            read, write = os.pipe()
            try:
                os.write(write, partial)
                if close_writer:
                    os.close(write); write = None
                with ExitStack() as stack:
                    child = Mock(pid=42, returncode=2); child.poll.return_value = 2
                    child.stdout = os.fdopen(read, 'rb', buffering=0)
                    stack.enter_context(patch('measurement_clock_conformance.sys.platform', 'darwin'))
                    stack.enter_context(patch('measurement_clock_conformance.time.get_clock_info', return_value=SimpleNamespace(implementation='mach_absolute_time()')))
                    stack.enter_context(patch('measurement_clock_conformance.executable_binding', return_value={'sha256': 'hash', 'identity': {}}))
                    stack.enter_context(patch('measurement_clock_conformance.subprocess.Popen', return_value=child))
                    stack.enter_context(patch('measurement_clock_conformance.read_bounded_line', side_effect=lambda stream: read_bounded_line(stream, .01)))
                    result = diagnose(Path('/mock/installed'))
                    self.assertEqual(result['verdict'], 'FAILED_CLOCK_CONFORMANCE')
                    self.assertIn(message, result['error'])
                    self.assertEqual(bytes.fromhex(result['failed_response']['data']), partial)
                    self.assertLessEqual(result['failed_response']['bytes'], 4096)
                    self.assertEqual(json.loads(json.dumps(result))['failed_response'], result['failed_response'])
            finally:
                if write is not None: os.close(write)

    def test_partial_line_has_total_deadline(self):
        read, write = os.pipe()
        try:
            os.write(write, b'{"partial":')
            with os.fdopen(read, 'rb', buffering=0) as stream:
                with self.assertRaisesRegex(ValueError, 'timeout'):
                    read_bounded_line(stream, seconds=.01)
        finally:
            os.close(write)

    def test_missing_executable_produces_structured_failure(self):
        result = diagnose(Path('/nonexistent/s12-clock-test'))
        self.assertEqual(result['verdict'], 'FAILED_CLOCK_CONFORMANCE')
        self.assertIn('error', result)

    def test_identity_hash_and_runtime_binding_without_launch(self):
        for mismatch in (False, True):
            with ExitStack() as stack:
                child = Mock(pid=42, returncode=0); child.wait.return_value = 0; child.poll.return_value = 0
                rows = [json.dumps({'clock': CLOCK, 'native_pid': 42, 'run_id': 'conformance', 'anchor_id': i + 1,
                                    'session_id': 'wrong' if mismatch and i == 1 else 'session', 'monotonic_ns': str(i * 100 + 10)}).encode() + b'\n' for i in range(16)]
                stack.enter_context(patch('measurement_clock_conformance.sys.platform', 'darwin'))
                stack.enter_context(patch('measurement_clock_conformance.time.get_clock_info', return_value=type('Clock', (), {})()))
                # An explicit namespace records clock metadata in the output receipt.
                from types import SimpleNamespace
                stack.enter_context(patch('measurement_clock_conformance.time.get_clock_info', return_value=SimpleNamespace(implementation='mach_absolute_time()', monotonic=True)))
                stack.enter_context(patch('measurement_clock_conformance.time.monotonic_ns', side_effect=[v for i in range(16) for v in (i * 100, i * 100 + 20)]))
                stack.enter_context(patch('measurement_clock_conformance.executable_binding', return_value={'sha256': 'hash', 'identity': {}}))
                stack.enter_context(patch('measurement_clock_conformance.subprocess.Popen', return_value=child))
                stack.enter_context(patch('measurement_clock_conformance.read_bounded_line', side_effect=rows))
                result = diagnose(Path('/mock/installed'))
                self.assertEqual(result['verdict'], 'FAILED_CLOCK_CONFORMANCE' if mismatch else 'PASS_CLOCK_CONFORMANCE_ONLY')
                self.assertEqual(result['executable_before'], result['executable_after'])
                self.assertIn('python', result)


if __name__ == '__main__':
    unittest.main()
