import copy
import fcntl
import json
import hashlib
import subprocess
import os
import sys
from contextlib import ExitStack
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import Mock, patch

import observe_import_native_v3 as observer
from evaluate_import_overlap_v3 import COUNTERS, evaluate


def fixture():
    declaration = {'root_pid': 42, 'root_birth_unix_s': 10, 'desktop_pid': 43, 'desktop_birth_unix_s': 11,
                   'filesystem_pid': 44, 'filesystem_birth_unix_s': 12, 'executable': '/tmp/photo',
                   'executable_sha256': 'a' * 64, 'executable_identity': {'st_dev': 1, 'st_ino': 2, 'st_size': 3, 'st_mtime_ns': 4, 'st_ctime_ns': 5, 'st_mode': 33261},
                   'catalog': '/tmp/catalog', 'catalog_identity': [1, 3], 'lock_identity': [1, 4],
                   'import_id': '12345678-1234-4123-8123-123456789012', 'source_blake3': 'b' * 64,
                   'run_id': 'run', 'setup_cutoff_ordinal': 0, 'input_ordinals': list(range(1, 101)), 'input_kind': 'edit', 'input_action': 'edit'}
    event_number = 0
    def event():
        nonlocal event_number
        event_number += 1
        return event_number
    def anchor(index):
        return {'anchor_id': index, 'send_event': event(), 'receive_event': event(), 'error': None,
                'native': {'run_id': 'run', 'anchor_id': index, 'session_id': 'session', 'native_pid': 42,
                           'clock': observer.CLOCK, 'monotonic_ns': str(index * 1000)}}
    def status(index):
        return {'request_event': event(), 'event': event(), 'binding': 1, 'phase': 'discovering', **{field: str(index if field == 'imported' else 0) for field in COUNTERS}, 'pending_previews': 0}
    anchors, timeline, samples, events = [anchor(1)], [status(0)], [], []
    for index in range(1, 101):
        events.append({'ordinal': index, 'start_event': event(), 'durable_event': event(), 'end_event': event()})
        samples.append({'ordinal': index, 'kind': 'edit', 'during_import': True, 'import_id': declaration['import_id'],
                        'outcome': 'complete', 'durable_us': 100000, 'presentation_us': 150000})
        timeline.append(status(index))
        anchors.append(anchor(index + 1))
    receipt = {'protocol': 2, 'run_id': 'run', 'overflowed': 0, 'samples': samples,
               'clock_alignment': {'model': 'causal_native_brackets_v1', 'profile': 'import_v1', 'interval_ms': 200,
                                   'duration_ms': 600000, 'stop_reason': 'finalized', 'anchors': anchors, 'sample_events': events,
                                   'import_evidence': {'bindings': [{'key': 1, 'id': declaration['import_id'], 'source_blake3': declaration['source_blake3']}],
                                                       'timeline': timeline, 'overflowed': 0}}}
    processes = {role: {'pid': declaration[f'{role}_pid'], 'birth_unix_s': declaration[f'{role}_birth_unix_s'],
                        'parent_pid': 1 if role == 'root' else 42, 'argv': ['/tmp/photo'] + ([] if role == 'root' else [observer.ROLES[role]]),
                        'executable_identity': declaration['executable_identity']} for role in ('root', 'desktop', 'filesystem')}
    proof = {'processes': processes, 'lock_identity': [1, 4], 'lock_contended_before': True, 'lock_contended_after': True,
             'role_owners': {role: [{'pid': declaration[f'{role}_pid'], 'birth_unix_s': declaration[f'{role}_birth_unix_s']}] for role in observer.ROLES},
             'holder': {'pid': 44, 'path': '/tmp/catalog/import.lock', 'device_inode': [1, 4], 'sole_visible_owner': True, 'descriptors': [{'fd': '5', 'lock_field': ' '}], 'elapsed_ns': 100}}
    segment = {'segment': 1, 'first_positive_after_monotonic_ns': 1000, 'last_positive_before_monotonic_ns': 101000,
               'positive_observations': 2, 'closed_reason': 'observer_end', 'closed_monotonic_ns': 102000}
    rows = [{'kind': 'identity', 'protocol': 3, 'clock': observer.CLOCK, 'profile': 'import_v1',
             **{key: declaration[key] for key in observer.BINDINGS}, 'monotonic_ns': 0, 'seconds': 600, 'interval': .2, 'declaration_sha256': observer.declaration_digest(declaration), 'clock_implementation': 'mach_absolute_time()'},
            {'kind': 'segment_admitted', 'segment': 1, 'positive_before_monotonic_ns': 999, 'positive_after_monotonic_ns': 1000, **copy.deepcopy(proof)},
            {'kind': 'active', 'segment': 1, 'positive_before_monotonic_ns': 101000, 'positive_after_monotonic_ns': 101001, **copy.deepcopy(proof)},
            {'kind': 'segment_closed', 'segment': 1, 'reason': 'observer_end', 'monotonic_ns': 102000},
            {'kind': 'summary', 'protocol': 3, 'clock': observer.CLOCK, 'fatal_errors': 0, 'root_same_birth': True, 'executable_unchanged': True,
             'measurement_end_monotonic_ns': 102000, 'segments': [segment], 'positive_observations': 2, 'observation_gaps': 0, 'usable_segments': 1}]
    return receipt, rows, declaration


def idle_warmup_fixture(kind='edit', explicit_null=True):
    receipt, rows, declaration = fixture()
    alignment = receipt['clock_alignment']
    for anchor in alignment['anchors']:
        anchor['send_event'] += 2
        anchor['receive_event'] += 2
    for row in alignment['sample_events']:
        row['ordinal'] += 1
        for key in ('start_event', 'durable_event', 'end_event'): row[key] += 2
    for row in alignment['import_evidence']['timeline']:
        row['request_event'] += 2
        row['event'] += 2
    for sample in receipt['samples']: sample['ordinal'] += 1
    warmup = {'ordinal': 1, 'kind': kind, 'during_import': False, 'outcome': 'complete',
              'durable_us': 250000, 'presentation_us': 300000}
    if explicit_null: warmup['import_id'] = None
    receipt['samples'].insert(0, warmup)
    alignment['sample_events'].insert(0, {'ordinal': 1, 'start_event': 1, 'end_event': 2})
    declaration.update(setup_cutoff_ordinal=1, input_ordinals=list(range(2, 102)))
    rows[0]['declaration_sha256'] = observer.declaration_digest(declaration)
    return receipt, rows, declaration


class EvaluatorTests(unittest.TestCase):
    def test_exact100_boundary_and_budget(self):
        result = evaluate(*fixture())
        self.assertEqual(result['verdict'], 'TIMING_PASS_REQUIRES_IMPORT_RECONCILIATION')
        self.assertEqual(result['cohort'], list(range(1, 101)))
        self.assertEqual(result['durable_p95_ms'], 100)
        self.assertTrue(result['native_progress_event_pairs'])

    def test_idle_warmup_then_100_import_inputs_keeps_exact_cohort(self):
        for kind in ('cull', 'edit'):
            for explicit_null in (True, False):
                values = idle_warmup_fixture(kind, explicit_null)
                result = evaluate(*values)
                self.assertEqual(result['verdict'], 'TIMING_PASS_REQUIRES_IMPORT_RECONCILIATION')
                self.assertEqual(result['cohort'], list(range(2, 102)))
                self.assertEqual(result['durable_p95_ms'], 100)
                self.assertEqual(values[0]['samples'][0]['durable_us'], 250000)
                self.assertNotIn('durable_event', values[0]['clock_alignment']['sample_events'][0])

    def test_missing_import_cohort_durable_event_still_rejects_after_idle_warmup(self):
        values = idle_warmup_fixture()
        values[0]['clock_alignment']['sample_events'][1].pop('durable_event')
        with self.assertRaisesRegex(ValueError, 'Durable event/timing mismatch'): evaluate(*values)

    def test_nonimport_setup_recorded_optional_event_requires_latency(self):
        values = idle_warmup_fixture()
        # The clock has already started, but setup context is non-import.
        # Preserve its legitimately recorded event; the cohort stays unchanged.
        for anchor in values[0]['clock_alignment']['anchors']:
            anchor['send_event'] += 1; anchor['receive_event'] += 1
        for row in values[0]['clock_alignment']['import_evidence']['timeline']:
            row['request_event'] += 1; row['event'] += 1
        for row in values[0]['clock_alignment']['sample_events'][1:]:
            for key in ('start_event', 'durable_event', 'end_event'): row[key] += 1
        values[0]['clock_alignment']['anchors'][0].update(send_event=1, receive_event=2)
        values[0]['clock_alignment']['sample_events'][0].update(start_event=3, durable_event=4, end_event=5)
        result = evaluate(*values)
        self.assertEqual(result['verdict'], 'TIMING_PASS_REQUIRES_IMPORT_RECONCILIATION')
        self.assertEqual(result['cohort'], list(range(2, 102)))
        values[0]['samples'][0]['durable_us'] = None
        with self.assertRaisesRegex(ValueError, 'Durable event without latency'): evaluate(*values)

    def test_all_latencies_retained_no_favorable_selection(self):
        values = fixture()
        for sample in values[0]['samples'][-6:]: sample['durable_us'] = 101000
        result = evaluate(*values)
        self.assertEqual(result['verdict'], 'FAILED_LATENCY')
        self.assertEqual(len(result['decisions']), 100)

    def test_rating_cull_mapping(self):
        values = fixture(); values[2].update(input_kind='cull', input_action='rating')
        for sample in values[0]['samples']: sample['kind'] = 'cull'
        values[1][0]['declaration_sha256'] = observer.declaration_digest(values[2])
        self.assertTrue(evaluate(*values)['verdict'].startswith('TIMING_PASS'))
        values[2]['input_action'] = 'reject'
        with self.assertRaises(ValueError): evaluate(*values)

    def test_wall_epoch_and_presentation_do_not_qualify_durable(self):
        values = fixture(); baseline = evaluate(*values)
        values[0]['time_origin_ms'] = -1e99; values[1][0]['wall_ns'] = 1e99
        values[0]['clock_alignment']['sample_events'][-1]['end_event'] = 8000
        values[0]['samples'][-1]['presentation_us'] = 9000000
        self.assertEqual(evaluate(*values), baseline)

    def test_first_last_anchor_missing_and_delayed_ack_are_partial(self):
        for mutate in (lambda a: a[0]['native'].update(monotonic_ns='999'),
                       lambda a: a[-1]['native'].update(monotonic_ns='101001'),
                       lambda a: a[-1].update(receive_event=None, native=None, error='incomplete')):
            values = fixture(); mutate(values[0]['clock_alignment']['anchors'])
            self.assertEqual(evaluate(*values)['verdict'], 'PARTIAL')

    def test_fixed_cohort_missing_tail_partial_and_extras_invalid(self):
        values = fixture(); values[0]['samples'].pop(); values[0]['clock_alignment']['sample_events'].pop()
        self.assertEqual(evaluate(*values)['verdict'], 'PARTIAL')
        for mutate in (lambda v: v[2]['input_ordinals'].__setitem__(-1, 101),
                       lambda v: v[0]['samples'].append({**v[0]['samples'][-1], 'ordinal': 101}),
                       lambda v: v[0]['samples'][0].update(ordinal=2),
                       lambda v: v[0]['samples'][0].update(kind='cull')):
            values = fixture(); mutate(values)
            with self.assertRaises(ValueError): evaluate(*values)

    def test_input_failure_is_not_hidden_by_durable_latency(self):
        values = fixture(); values[0]['samples'][49]['outcome'] = 'failed'
        self.assertEqual(evaluate(*values)['verdict'], 'FAILED_ACTIVE_INPUT')

    def test_context_uuid_source_overflow_and_profile_mutations(self):
        for mutate in (lambda r: r['samples'][0].update(import_id=None),
                       lambda r: r['samples'][0].update(during_import=False),
                       lambda r: r['clock_alignment'].update(profile='export_v1'),
                       lambda r: r['clock_alignment'].update(interval_ms=100),
                       lambda r: r['clock_alignment']['import_evidence'].update(overflowed=1),
                       lambda r: r['clock_alignment']['import_evidence']['bindings'][0].update(source_blake3='c' * 64),
                       lambda r: r['clock_alignment']['import_evidence']['timeline'][1].update(binding=2),
                       lambda r: r['clock_alignment']['sample_events'][0].pop('durable_event')):
            values = fixture(); mutate(values[0])
            with self.assertRaises(ValueError): evaluate(*values)

    def test_anchor_identity_order_session_and_duplicate_event(self):
        for mutate in (lambda a: a['anchors'][0]['native'].update(native_pid=43),
                       lambda a: a['anchors'][1]['native'].update(session_id='other'),
                       lambda a: a['anchors'][1]['native'].update(monotonic_ns='999'),
                       lambda a: a['anchors'][1].update(send_event=1),
                       lambda a: a['sample_events'][0].update(durable_event=1),
                       lambda a: a['import_evidence']['timeline'][0].update(event=1)):
            values = fixture(); mutate(values[0]['clock_alignment'])
            with self.assertRaises(ValueError): evaluate(*values)

    def test_lock_without_progress_and_missing_status_context_partial(self):
        for mode in ('no_progress', 'no_pre', 'no_post'):
            values = fixture(); timeline = values[0]['clock_alignment']['import_evidence']['timeline']
            if mode == 'no_progress':
                for row in timeline: row['imported'] = '0'
            elif mode == 'no_pre': timeline.pop(0)
            else: timeline.pop()
            self.assertEqual(evaluate(*values)['verdict'], 'PARTIAL')

    def test_progress_without_lock_is_partial(self):
        values = fixture(); values[1][1:-1] = []
        values[1][-1].update(segments=[], positive_observations=0, usable_segments=0)
        self.assertEqual(evaluate(*values)['verdict'], 'PARTIAL')

    def test_progress_outside_active_segment_cannot_qualify(self):
        values = fixture(); timeline = values[0]['clock_alignment']['import_evidence']['timeline']
        for row in timeline[1:]: row['imported'] = '1'
        # Only progress is between pre-cohort and first status, not in cohort.
        self.assertEqual(evaluate(*values)['verdict'], 'PARTIAL')

    def test_delayed_precohort_progress_response_does_not_prove_incohort_work(self):
        values = fixture(); timeline = values[0]['clock_alignment']['import_evidence']['timeline']
        # Only increase occurs between a snapshot requested before the first
        # input and a later in-cohort snapshot: full progress interval is not
        # causally contained in the cohort and cannot qualify useful work.
        for row in timeline[1:]: row['imported'] = '1'
        self.assertEqual(evaluate(*values)['verdict'], 'PARTIAL')
        values = fixture(); timeline = values[0]['clock_alignment']['import_evidence']['timeline']
        timeline[1]['request_event'] = timeline[0]['event']
        with self.assertRaisesRegex(ValueError, 'event|sequence'): evaluate(*values)
        values = fixture(); timeline = values[0]['clock_alignment']['import_evidence']['timeline']
        timeline[1]['request_event'] = timeline[1]['event'] + 1
        with self.assertRaises(ValueError): evaluate(*values)

    def test_raw_identity_role_ancestry_and_holder_mutations(self):
        mutations = [lambda p: p['processes']['filesystem'].update(birth_unix_s=13),
                     lambda p: p['processes']['filesystem'].update(parent_pid=43),
                     lambda p: p['processes']['desktop'].update(argv=['/tmp/photo', '--wrong-role']),
                     lambda p: p['processes']['root'].update(executable_identity={}),
                     lambda p: p.update(lock_identity=[1, 9]),
                     lambda p: p.update(lock_contended_before=False),
                     lambda p: p['role_owners']['desktop'].append({'pid': 45, 'birth_unix_s': 14}),
                     lambda p: p['holder'].update(pid=43),
                     lambda p: p['holder'].update(device_inode=[1, 9]),
                     lambda p: p['holder']['descriptors'][0].update(lock_field='R'),
                     lambda p: p['holder'].update(sole_visible_owner=False),
                     lambda p: p['holder'].update(path='/tmp/other')]
        for mutate in mutations:
            values = fixture(); mutate(values[1][2])
            with self.assertRaises(ValueError): evaluate(*values)

    def test_raw_summary_bijection_counts_and_boundaries(self):
        for mutate in (lambda rows: rows.insert(2, {**rows[1], 'segment': 2}),
                       lambda rows: rows[-1].update(segments=[]),
                       lambda rows: rows[-1].update(positive_observations=3),
                       lambda rows: rows[-1].update(observation_gaps=1),
                       lambda rows: rows[-1].update(usable_segments=0),
                       lambda rows: rows[-1]['segments'][0].update(last_positive_before_monotonic_ns=101001),
                       lambda rows: rows[-2].update(monotonic_ns=101999),
                       lambda rows: rows.insert(2, {'kind': 'observation_gap', 'reason': 'lock_not_contended', 'monotonic_ns': 1500})):
            values = fixture(); mutate(values[1])
            with self.assertRaises(ValueError): evaluate(*values)

    def test_gap_closes_no_bridge_and_error_never_passes(self):
        values = fixture(); rows = values[1]
        rows[-2].update(reason='lock_not_contended', monotonic_ns=101002)
        rows[-1]['segments'][0].update(closed_reason='lock_not_contended', closed_monotonic_ns=101002)
        rows.insert(-1, {'kind': 'observation_gap', 'reason': 'lock_not_contended', 'monotonic_ns': 101002})
        rows[-1]['observation_gaps'] = 1
        self.assertTrue(evaluate(*values)['verdict'].startswith('TIMING_PASS'))
        rows.insert(-1, {'kind': 'error', 'monotonic_ns': 101003})
        with self.assertRaises(ValueError): evaluate(*values)

    def test_cli_hashes_inputs_and_refuses_overwrite(self):
        values = fixture()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); receipt, rows, declaration = values
            paths = {name: root / name for name in ('receipt', 'observer', 'declaration', 'output')}
            paths['receipt'].write_text(json.dumps(receipt))
            paths['observer'].write_text('\n'.join(json.dumps(row) for row in rows) + '\n')
            paths['declaration'].write_text(json.dumps(declaration))
            command = [sys.executable, str(Path(__file__).with_name('evaluate_import_overlap_v3.py'))]
            for name, path in paths.items(): command.extend(['--' + name, str(path)])
            result = subprocess.run(command, capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            output = paths['output'].read_bytes(); evidence = json.loads(output)
            self.assertEqual(evidence['inputs']['receipt']['sha256'], hashlib.sha256(paths['receipt'].read_bytes()).hexdigest())
            self.assertNotEqual(subprocess.run(command, capture_output=True, timeout=5).returncode, 0)
            self.assertEqual(paths['output'].read_bytes(), output)



class ObserverTests(unittest.TestCase):
    def patched_run(self, probes):
        declaration = fixture()[2]; emitted = []; tick = iter(range(0, 100000000, 1000000))
        with ExitStack() as stack:
            stack.enter_context(patch.object(observer.sys, 'platform', 'darwin'))
            stack.enter_context(patch.object(observer.time, 'get_clock_info', return_value=SimpleNamespace(implementation='mach_absolute_time()')))
            stack.enter_context(patch.object(observer.time, 'monotonic_ns', side_effect=lambda: next(tick)))
            stack.enter_context(patch.object(observer.time, 'sleep'))
            stack.enter_context(patch.object(observer, 'positive_probe', side_effect=probes))
            stack.enter_context(patch.object(observer, 'sha256_path', return_value='a' * 64))
            stack.enter_context(patch.object(observer, 'executable_identity', return_value=declaration['executable_identity']))
            stack.enter_context(patch.object(observer, 'same_birth', return_value=True))
            code = observer.observe(declaration, lambda row: emitted.append(copy.deepcopy(row)), seconds=3)
        return code, emitted

    def test_contention_loss_retires_and_never_reopens(self):
        proof = {'positive_before_monotonic_ns': 1, 'positive_after_monotonic_ns': 2}
        code, rows = self.patched_run([proof, proof, None, AssertionError('Must not reopen')])
        self.assertEqual(code, 0)
        self.assertEqual(rows[-1]['positive_observations'], 2)
        self.assertEqual([r['kind'] for r in rows][-3:], ['segment_closed', 'observation_gap', 'summary'])

    def test_access_and_systemerror_keep_cause_and_complete_summary(self):
        cause = PermissionError(13, 'denied')
        system = SystemError('proc_cmdline failed'); system.__cause__ = cause
        for error in (observer.psutil.AccessDenied(44), system, RuntimeError('unknown failure')):
            code, rows = self.patched_run([{'positive_before_monotonic_ns': 1, 'positive_after_monotonic_ns': 2}, error])
            self.assertEqual(code, 2)
            self.assertEqual([r['kind'] for r in rows][-3:], ['segment_closed', 'error', 'summary'])
            self.assertEqual(rows[-1]['fatal_errors'], 1)
            self.assertEqual(rows[-2]['exception_chain'][0]['type'], type(error).__name__)
            if error is system: self.assertEqual(rows[-2]['exception_chain'][1]['type'], 'PermissionError')
            self.assertFalse(any(r['kind'] == 'observation_gap' for r in rows))

    def test_late_probe_is_fatal_and_never_admitted(self):
        code, rows = self.patched_run([{'positive_before_monotonic_ns': 1, 'positive_after_monotonic_ns': 3000000001}])
        self.assertEqual(code, 2)
        self.assertEqual(rows[-1]['positive_observations'], 0)
        self.assertIn('exceeded observer deadline', rows[-2]['error'])

    def test_invalid_preflight_writes_error_and_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); declaration = root / 'declaration.json'; output = root / 'observer.jsonl'
            declaration.write_text('{}')
            result = subprocess.run([sys.executable, str(Path(__file__).with_name('observe_import_native_v3.py')),
                                     '--declaration', str(declaration), '--output', str(output)], capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 2)
            rows = [json.loads(row) for row in output.read_text().splitlines()]
            self.assertEqual([row['kind'] for row in rows], ['error', 'summary'])
            self.assertEqual(rows[-1]['fatal_errors'], 1)

    def test_exact_lsof_inode_device_lock_and_owner(self):
        output = 'p44\nf5\nlW\nD0x1\ni4\nn/tmp/catalog/import.lock\n'
        with patch.object(observer.subprocess, 'run', return_value=SimpleNamespace(returncode=0, stdout=output, stderr='')):
            value = observer.import_lock_holder(44, Path('/tmp/catalog/import.lock'), [1, 4])
            self.assertTrue(value['sole_visible_owner'])
        for bad in (output.replace('p44', 'p45'), output.replace('i4', 'i9'), output.replace('D0x1', 'D0x2'),
                    output.replace('lW', 'lR'), output.replace('/tmp/catalog', '/tmp/other'), output + 'p45\nf6\nlW\nD0x1\ni4\nn/tmp/catalog/import.lock\n'):
            with patch.object(observer.subprocess, 'run', return_value=SimpleNamespace(returncode=0, stdout=bad, stderr='')):
                with self.assertRaises(ValueError): observer.import_lock_holder(44, Path('/tmp/catalog/import.lock'), [1, 4])

    @unittest.skipUnless(sys.platform == 'darwin' and Path('/usr/sbin/lsof').exists(), 'Darwin lsof lock conformance')
    def test_darwin_lsof_recognizes_own_temporary_flock(self):
        # No product process/import is started or queried: qualify lsof output
        # against one temporary file owned exclusively by this unit test.
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory).resolve() / 'import.lock'
            with path.open('w') as lock:
                fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                identity = observer.file_identity(path)
                self.assertTrue(observer.lock_contended(path, tuple(identity)))
                holder = observer.import_lock_holder(os.getpid(), path, identity)
                self.assertTrue(holder['sole_visible_owner'])
                self.assertIn(holder['descriptors'][0]['lock_field'], (' ', 'W'))
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
                self.assertFalse(observer.lock_contended(path, tuple(identity)))

    def test_lsof_timeout_error_and_overflow_are_fatal(self):
        for result in (SimpleNamespace(returncode=1, stdout='', stderr=''), SimpleNamespace(returncode=0, stdout='x' * 17000, stderr=''),
                       SimpleNamespace(returncode=0, stdout='', stderr='denied')):
            with patch.object(observer.subprocess, 'run', return_value=result):
                with self.assertRaises(ValueError): observer.import_lock_holder(44, Path('/tmp/catalog/import.lock'), [1, 4])

    def test_process_wrong_parent_role_executable_and_pid_reuse(self):
        declaration = fixture()[2]
        process = Mock(); process.exe.return_value = '/tmp/photo'; process.cmdline.return_value = ['/tmp/photo', observer.ROLES['filesystem']]; process.ppid.return_value = 42
        with patch.object(observer.psutil, 'Process', return_value=process), patch.object(observer, 'same_birth', return_value=True) as born, patch.object(observer, 'executable_identity', return_value=declaration['executable_identity']):
            self.assertEqual(observer.process_proof(declaration, 'filesystem')['pid'], 44)
            process.ppid.return_value = 43
            with self.assertRaises(ValueError): observer.process_proof(declaration, 'filesystem')
            process.ppid.return_value = 42; process.cmdline.return_value = ['/tmp/photo', '--other']
            with self.assertRaises(ValueError): observer.process_proof(declaration, 'filesystem')
            process.cmdline.return_value = ['/tmp/photo', observer.ROLES['filesystem']]; process.exe.return_value = '/tmp/replaced'
            with self.assertRaises(ValueError): observer.process_proof(declaration, 'filesystem')
            process.exe.return_value = '/tmp/photo'; born.return_value = False
            with self.assertRaises(ValueError): observer.process_proof(declaration, 'filesystem')

    def test_duplicate_role_owner_and_missing_owner_reject(self):
        declaration = fixture()[2]
        root = Mock()
        children = []
        for role in observer.ROLES:
            child = Mock(); child.pid = declaration[f'{role}_pid']
            child.cmdline.return_value = ['/tmp/photo', observer.ROLES[role]]
            child.ppid.return_value = 42; child.create_time.return_value = declaration[f'{role}_birth_unix_s']
            children.append(child)
        root.children.return_value = children
        with patch.object(observer.psutil, 'Process', return_value=root):
            self.assertEqual(len(observer.unique_role_owners(declaration)), 2)
            root.children.return_value = children + [children[0]]
            with self.assertRaisesRegex(ValueError, 'Ambiguous'): observer.unique_role_owners(declaration)
            root.children.return_value = children[:1]
            with self.assertRaisesRegex(ValueError, 'missing'): observer.unique_role_owners(declaration)

    def test_departed_unbound_child_cmdline_denial_does_not_hide_exact_role_owners(self):
        declaration = fixture()[2]
        root = Mock()
        owners = []
        for role in observer.ROLES:
            child = Mock(pid=declaration[f'{role}_pid'])
            child.create_time.return_value = declaration[f'{role}_birth_unix_s']
            child.cmdline.return_value = ['/tmp/photo', observer.ROLES[role]]
            child.ppid.return_value = declaration['root_pid']
            owners.append(child)
        helper = Mock(pid=45)
        helper.create_time.return_value = 13
        denial = observer.psutil.AccessDenied(45)
        denial.__cause__ = PermissionError(
            13, 'force permission denied (originated from sysctl(KERN_PROCARGS2) -> EINVAL)')
        helper.cmdline.side_effect = denial
        root.children.return_value = [*owners, helper]
        with patch.object(observer.psutil, 'Process', side_effect=lambda pid: root if pid == 42 else Mock(
                is_running=Mock(return_value=True), status=Mock(return_value=observer.psutil.STATUS_ZOMBIE))):
            self.assertEqual(observer.unique_role_owners(declaration), {
                role: [{'pid': declaration[f'{role}_pid'],
                        'birth_unix_s': declaration[f'{role}_birth_unix_s']}]
                for role in observer.ROLES
            })

    def test_live_or_uncertain_unbound_child_cmdline_denial_remains_fatal(self):
        declaration = fixture()[2]
        for status_error in (None, observer.psutil.AccessDenied(45)):
            root = Mock()
            helper = Mock(pid=45)
            helper.create_time.return_value = 13
            denial = observer.psutil.AccessDenied(45)
            helper.cmdline.side_effect = denial
            root.children.return_value = [helper]
            fresh = Mock()
            fresh.is_running.return_value = True
            if status_error is None:
                fresh.status.return_value = 'running'
                fresh.create_time.return_value = 13
            else:
                fresh.status.side_effect = status_error
            with patch.object(observer.psutil, 'Process', side_effect=lambda pid: root if pid == 42 else fresh):
                with self.assertRaises(observer.psutil.AccessDenied) as raised:
                    observer.unique_role_owners(declaration)
                self.assertIs(raised.exception, denial)

    def test_unbound_child_birth_denial_remains_fatal(self):
        declaration = fixture()[2]
        root = Mock()
        helper = Mock(pid=45)
        denial = observer.psutil.AccessDenied(45)
        helper.create_time.side_effect = denial
        root.children.return_value = [helper]
        with patch.object(observer.psutil, 'Process', return_value=root), \
             patch.object(observer, 'same_birth') as lifecycle:
            with self.assertRaises(observer.psutil.AccessDenied) as raised:
                observer.unique_role_owners(declaration)
            self.assertIs(raised.exception, denial)
            lifecycle.assert_not_called()

    def test_declared_owner_cmdline_failure_cannot_be_ignored_as_departed_helper(self):
        declaration = fixture()[2]
        root = Mock()
        desktop = Mock(pid=declaration['desktop_pid'])
        desktop.create_time.return_value = declaration['desktop_birth_unix_s']
        desktop.cmdline.side_effect = observer.psutil.AccessDenied(desktop.pid)
        root.children.return_value = [desktop]
        with patch.object(observer.psutil, 'Process', return_value=root), \
             patch.object(observer, 'same_birth') as lifecycle:
            with self.assertRaises(observer.psutil.AccessDenied):
                observer.unique_role_owners(declaration)
            lifecycle.assert_not_called()

    def test_departed_declared_owner_is_still_missing(self):
        declaration = fixture()[2]
        root = Mock()
        desktop = Mock(pid=declaration['desktop_pid'])
        desktop.create_time.side_effect = observer.psutil.NoSuchProcess(desktop.pid)
        root.children.return_value = [desktop]
        with patch.object(observer.psutil, 'Process', return_value=root):
            with self.assertRaisesRegex(ValueError, 'missing'):
                observer.unique_role_owners(declaration)

    def test_file_identity_rejects_symlink_and_replacement(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory).resolve() / 'lock'; path.write_text('')
            identity = observer.file_identity(path)
            alias = path.with_name('alias'); alias.symlink_to(path)
            with self.assertRaises(ValueError): observer.file_identity(alias)
            replacement = path.with_name('new'); replacement.write_text(''); replacement.replace(path)
            self.assertNotEqual(observer.file_identity(path), identity)

    def test_probe_rechecks_after_lsof_and_contention_loss(self):
        declaration = fixture()[2]
        with patch.object(observer, 'executable_identity', return_value=declaration['executable_identity']), \
             patch.object(observer, 'file_identity', side_effect=[[1, 3], [1, 4], [1, 3], [1, 9]]), \
             patch.object(observer, 'process_proof', return_value={}), \
             patch.object(observer, 'unique_role_owners', return_value={}), \
             patch.object(observer, 'lock_contended', return_value=True), \
             patch.object(observer, 'import_lock_holder', return_value={}):
            with self.assertRaisesRegex(ValueError, 'lock changed'): observer.positive_probe(declaration)
        with patch.object(observer, 'executable_identity', return_value=declaration['executable_identity']), \
             patch.object(observer, 'file_identity', side_effect=[[1, 3], [1, 4], [1, 3], [1, 4]]), \
             patch.object(observer, 'process_proof', return_value={}), \
             patch.object(observer, 'unique_role_owners', return_value={}), \
             patch.object(observer, 'lock_contended', side_effect=[True, False]), \
             patch.object(observer, 'import_lock_holder', return_value={}):
            self.assertIsNone(observer.positive_probe(declaration))


if __name__ == '__main__':
    unittest.main()
