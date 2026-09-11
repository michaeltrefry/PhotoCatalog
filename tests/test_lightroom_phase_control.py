"""Small synthetic contracts only; no native inspector, SQLite, or source photos."""
import importlib.util
import errno
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import types
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
def load(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT/'scripts'/file)
    value = importlib.util.module_from_spec(spec); spec.loader.exec_module(value); return value
C = load('phase_contract_fixture', 'lightroom_phase_contract.py')
W = load('phase_control_fixture', 'lightroom_phase_control.py')
W.C = C


def stable():
    return {'before_complete': True, 'after_complete': True, 'added': [], 'removed': [], 'changed': []}


class PhaseContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.root = Path(self.temp.name).resolve()
        self.run = self.root/'run'; self.run.mkdir()
        for p in ['reports', 'plans', 'commands', 'steps', 'captures']: (self.run/p).mkdir()
        self.control = self.root/'control'; self.control.mkdir()
        self.binding = {'fixture': 'exact frozen binding'}
        self.config = {'catalog_root': str(self.root/'never-read-originals'), 'page_limit': 1000, 'resume_rows_per_call': 10000}
        self.config_ref = self.put(self.run/'config.json', self.config)
        self.keys = [C.sha(C.encoded({'encoding': 'UnixBytes', 'units': list(str(self.root/('never-read-'+str(i))).encode())})) for i in range(2)]
        self.main = {'automatic_selection': False, 'migration_executed': False, 'inventory_complete': True,
                     'inventory_delta': stable(), 'candidate_count': 2,
                     'outcomes': [{'key': k, 'candidate': {'path': {'encoding': 'UnixBytes', 'units': list(str(self.root/('never-read-'+str(i))).encode())}},
                                   'capture_path': str(self.root/('legacy-capture-'+k[0])), 'capture': {'state': 'captured', 'revision_id': 'r'+str(i)}, 'inspection': {'revision': 'r'+str(i)}} for i,k in enumerate(self.keys)],
                     'families': {'families': [{'id': 'family', 'suggested': 'r0', 'evidence_digest': 'e'*64,
                                               'members': [{'revision_id': 'r0'}, {'revision_id': 'r1'}]}]}}
        self.main_ref = self.put(self.run/'reports/main-review.json', self.main)
        request = {'main_review_sha256': self.main_ref['sha256'], 'automatic_selection': False,
                   'requests': [{'candidate_key': self.keys[0], 'revision': 'r0', 'family_evidence_digest': 'e'*64,
                                 'role': 'prospective_suggestion', 'reason': 'preserve full evidence'}], 'ambiguous_unselected': []}
        self.input_ref = self.put(self.root/'request.json', request)
        self.recipe = {'phase': 'full', 'run': str(self.run), 'input': self.input_ref, 'baseline': self.main_ref,
                       'paths_review': {'kind': 'not_applicable'}, 'expected_next_command': 1,
                       'config': self.config_ref, 'control': str(self.control), 'attempt_id': '00000000-0000-4000-8000-000000000001',
                       'binding': self.put(self.run/'binding.json', self.binding)}
        self.put(self.run/'journal.json', {'next_command': 1})
        self.attempt = self.control/'attempts'/self.recipe['attempt_id']; self.attempt.mkdir(parents=True)

    def tearDown(self): self.temp.cleanup()
    def put(self, path, value):
        path.write_bytes(C.encoded(value)); return C.reference(path)
    def ctx(self): return C.context(self.recipe)
    def full_output(self):
        ctx = self.ctx(); outcomes = []
        for i,k in enumerate(self.keys):
            full = {'ok': True, 'capture_path': str(self.run/'captures/000000002'),
                    'capture': {'state': 'captured', 'raw_byte_retention': 'complete', 'sqlite_consistency': 'consistent_default_sqlite', 'revision_id': 'new0'}} if i == 0 else None
            outcomes.append({'key': k, 'source': self.main['outcomes'][i]['candidate']['path'], 'full_requested': i == 0, 'full': full,
                             'inspection': {'revision': 'new0' if i == 0 else 'r1', 'capture_path': full['capture_path'] if full else self.main['outcomes'][i]['capture_path']}})
        return {'automatic_selection': False, 'migration_executed': False, 'request': ctx['input'], 'request_id': ctx['id'],
                'plan': ctx['plan'], 'inventory_delta': stable(), 'outcomes': outcomes,
                'outcome_counts': {'candidates': 2, 'full_requested': 1, 'full_capture_failures': 0, 'inspection_failures': 0, 'main_only_members': 1}}
    def paths_setup(self, phase='paths'):
        value = self.full_output(); ref = self.put(Path(self.ctx()['output']), value)
        self.recipe.update(phase=phase, input=ref, baseline=ref)
        return value, ref
    def path_output(self, value, ref):
        return {'automatic_selection': False, 'migration_executed': False, 'full_review_sha256': ref['sha256'],
                'inventory_delta': stable(), 'outcome_counts': {'requested_members': 1, 'failures': 0},
                'outcomes': [{'key': self.keys[0], 'revision': 'new0', 'report': {'revision_id': 'new0'},
                              'pages': {k: {'rows': 0} for k in ['paths', 'packets', 'metadata-conflicts', 'issues']}}]}
    def record(self, args, key, n=1, failure=None):
        path = self.run/'commands'/f'{n:09d}'; path.mkdir(exist_ok=True)
        record = {'sequence': n, 'source_binding': self.binding, 'requested_arguments': args, 'key': key,
                  'argv': [str(self.run/'lightroom_inspect'), *[str(self.run/'captures'/f'{n:09d}') if x == '@CAPTURE@' else x for x in args]],
                  'exit_code': 0 if failure is None else 1, 'failure': failure, 'log_errors': []}
        self.put(path/'result.json', record)
        self.put(self.run/'steps'/(C.sha(C.encoded(key))+'.json'), {'sequence': n, 'record': str((path/'result.json').relative_to(self.run))})
        self.put(self.run/'journal.json', {'next_command': n+1}); return record
    def full_admitted(self):
        key = ['phase-control', 'full', self.recipe['attempt_id'], 'discover']
        record = self.record(['discover', self.config['catalog_root']], key)
        stdout = self.put(self.run/'commands/000000001/stdout', {'complete': True, 'candidates': [v['candidate'] for v in self.main['outcomes']]})
        record['stdout'] = {'path': 'commands/000000001/stdout', 'sha256': stdout['sha256']}
        self.put(self.run/'commands/000000001/result.json', record)
        self.put(self.attempt/'inventory-admission.json', {'status': 'complete', 'key': key, 'command': 1, 'baseline': self.recipe['baseline'], 'binding': self.recipe['binding'], 'stdout': stdout})
    def phase(self, status, ctx, start=1):
        value = {'phase': self.recipe['phase'], 'input': self.recipe['input']['path'], 'binding': self.binding,
                 'started_unix': start, 'status': status, 'output': ctx['output']}
        return self.put(self.run/'reports/phase-fixture.json', value)

    def test_full_preserves_ambiguity_without_choosing_and_rejects_drift(self):
        value = C.document(self.input_ref)
        value['requests'].append({'candidate_key': self.keys[1], 'revision': 'r1', 'family_evidence_digest': 'e'*64,
                                  'role': 'additional_ambiguity_evidence', 'reason': 'inspect unresolved branch'})
        self.recipe['input'] = self.put(Path(self.input_ref['path']), value)
        self.assertEqual(len(self.ctx()['requested']), 2)
        value['requests'][1]['role'] = 'prospective_suggestion'
        self.recipe['input'] = self.put(Path(self.input_ref['path']), value)
        with self.assertRaisesRegex(ValueError, 'role'): self.ctx()
        self.main['inventory_delta']['changed'] = ['changed']
        self.put(Path(self.main_ref['path']), self.main)
        with self.assertRaisesRegex(ValueError, 'digest'): self.ctx()

    def test_full_result_fallback_wrong_revision_counts_and_changed_inventory_rejected(self):
        ctx = self.ctx(); value = self.full_output(); C.validate_output(value, ctx)
        for mutate in [lambda x: x['outcomes'][0]['full'].update(ok=False),
                       lambda x: x['outcomes'][0]['inspection'].update(revision='foreign'),
                       lambda x: x['outcome_counts'].update(candidates=1),
                       lambda x: x['outcomes'][1]['inspection'].update(revision='substituted'),
                       lambda x: x['outcomes'][1]['inspection'].update(capture_path='/foreign'),
                       lambda x: x['outcomes'][0].update(source={'encoding': 'UnixBytes', 'units': [1]}),
                       lambda x: x['inventory_delta'].update(after_complete=False)]:
            broken = json.loads(json.dumps(value)); mutate(broken)
            with self.assertRaises(ValueError): C.validate_output(broken, ctx)

    def test_paths_and_packets_require_exact_full_provenance_and_completed_path_review(self):
        value, ref = self.paths_setup(); ctx = self.ctx(); output = self.path_output(value, ref)
        C.validate_output(output, ctx)
        self.recipe['phase'] = 'packets'
        with self.assertRaises(ValueError): self.ctx()
        self.recipe['paths_review'] = self.put(self.run/'reports'/('paths-review-'+ref['sha256']+'.json'), output)
        self.assertEqual(self.ctx()['phase'], 'packets')
        output['outcomes'][0]['report']['revision_id'] = 'different'
        self.recipe['paths_review'] = self.put(Path(self.recipe['paths_review']['path']), output)
        with self.assertRaisesRegex(ValueError, 'wrong path'): self.ctx()

    def test_funding_complete_categories_exact_evidence_arithmetic_and_drift(self):
        source = self.put(self.root/'budget-basis.json', {'observed_bytes': 101, 'largest_command': 50})
        term = {'basis': [{'reference': source, 'pointer': ['observed_bytes']}], 'numerator': 3, 'denominator': 2,
                'reason': 'explicit reviewed growth allowance, not a measured maximum'}
        proof = {'protocol': 1, 'phase': 'full', 'input_sha256': self.input_ref['sha256'],
                 'categories': {k: term for k in C.CATEGORIES['full']}, 'single_command_headroom': dict(term),
                 'protected_bytes': 1000, 'reserve_bytes': C.RESERVE}
        ref = self.put(self.root/'budget.json', proof); result = C.funding(ref, 'full', self.input_ref['sha256'])
        self.assertEqual(result['phase_remaining_bytes'], 8*152)
        self.assertEqual(result['initial_minimum_bytes'], C.RESERVE+1000+8*152+152)
        proof['categories'].pop('all_row_pages'); ref = self.put(Path(ref['path']), proof)
        with self.assertRaisesRegex(ValueError, 'incomplete'): C.funding(ref, 'full', self.input_ref['sha256'])
        self.put(Path(source['path']), {'observed_bytes': 999})
        with self.assertRaisesRegex(ValueError, 'digest'): C.amount(term)
        with self.assertRaises(ValueError): C.integer(True)

    def test_actual_nested_full_command_keys_and_phase_specific_path_modes(self):
        ctx = self.ctx()
        args = ['rows', ctx['plan'], 'new0', '--after', '0', '--limit', '1000']
        self.record(args, [['full', ctx['id'], self.keys[0]], 'new0', 'rows', 0])
        W.command_record(self.recipe, ctx, 1, self.binding)
        value, ref = self.paths_setup('paths'); ctx = self.ctx()
        self.record(['check-paths', ctx['plan'], 'new0', '--limit', '1000'], [ctx['tag'], 'new0', 'check', 0])
        W.command_record(self.recipe, ctx, 1, self.binding)
        self.record(['check-paths', ctx['plan'], 'new0', '--limit', '1000', '--packets'], [ctx['tag'], 'new0', 'check', 0])
        with self.assertRaisesRegex(ValueError, 'mode'): W.command_record(self.recipe, ctx, 1, self.binding)

    def test_exact_returned_output_and_nonzero_pause_not_exit_code_alone(self):
        ctx = self.ctx(); self.put(Path(ctx['output']), self.full_output())
        self.phase('review_artifact_returned_not_acceptance', ctx)
        self.full_admitted()
        self.record(['families', ctx['plan']], ['full', ctx['id'], 'families'], n=2)
        result = W.classify(self.recipe, ctx, self.binding, set(), 0, 0, None)
        self.assertEqual(result['status'], 'review_returned_not_acceptance')
        with self.assertRaises(ValueError): W.classify(self.recipe, ctx, self.binding, set(), 0, 1, None)
        self.phase('paused', ctx)
        pause = self.put(self.run/'pause-request', {'owner': 'synthetic-test'})
        self.assertEqual(W.classify(self.recipe, ctx, self.binding, set(), 0, 1, pause)['status'], 'paused_at_command_boundary')
        self.record(['families', ctx['plan']], ['full', ctx['id'], 'families'], n=2, failure='deadline')
        with self.assertRaisesRegex(ValueError, 'native failure'): W.classify(self.recipe, ctx, self.binding, set(), 0, 1, pause)

    def test_orphan_tail_and_prohibited_command_never_qualify(self):
        ctx = self.ctx(); self.phase('paused', ctx); pause = self.put(self.run/'pause-request', {'owner': 'fixture'})
        self.put(self.run/'journal.json', {'next_command': 2})
        with self.assertRaises(FileNotFoundError): W.classify(self.recipe, ctx, self.binding, set(), 0, 1, pause)
        self.record(['choose', ctx['plan'], 'family', 'new0'], ['full', ctx['id'], 'choose'])
        with self.assertRaisesRegex(ValueError, 'prohibited'): W.command_record(self.recipe, ctx, 1, self.binding)

    def test_pause_capture_never_deletes_raced_replacement(self):
        attempt = self.root/'attempt'; attempt.mkdir()
        path = self.run/'pause-request'; ref = self.put(path, {'owner': 'old-owner'})
        m = path.stat(); self.recipe['pause'] = {'kind': 'owned', 'reference': ref, 'owner': 'old-owner',
            'identity': [m.st_dev, m.st_ino, m.st_size, m.st_mtime_ns, m.st_ctime_ns]}
        original_rename = os.rename
        def race(a,b):
            original_rename(a,b); path.write_bytes(b'foreign replacement')
        with mock.patch.object(W.os, 'rename', side_effect=race):
            with self.assertRaisesRegex(ValueError, 'raced'): W.remove_pause(self.recipe, attempt)
        self.assertEqual(path.read_bytes(), b'foreign replacement')
        self.assertEqual(C.sha((attempt/'pause-captured').read_bytes()), ref['sha256'])

    def cross_device_pause(self):
        path = self.run/'pause-request'; ref = self.put(path, {'owner': 'old-owner'})
        m = path.stat(); self.recipe['pause'] = {'kind': 'owned', 'reference': ref, 'owner': 'old-owner',
            'identity': [m.st_dev, m.st_ino, m.st_size, m.st_mtime_ns, m.st_ctime_ns]}
        actual_stat = Path.stat
        def device(path, *args, **kwargs):
            if path == self.attempt: return types.SimpleNamespace(st_dev=m.st_dev+1)
            return actual_stat(path, *args, **kwargs)
        return path, ref, mock.patch.object(Path, 'stat', device)

    def test_cross_device_pause_capture_stays_on_run_volume_with_local_receipt(self):
        path, ref, devices = self.cross_device_pause(); rename = os.rename
        def same_device_only(source, destination):
            if destination.parent == self.attempt: raise OSError(errno.EXDEV, 'simulated cross-device rename')
            self.assertEqual(destination.parent.parent, self.run)
            rename(source, destination)
        with devices, mock.patch.object(W.os, 'rename', side_effect=same_device_only), mock.patch.object(W, 'sync', wraps=W.sync) as sync:
            W.remove_pause(self.recipe, self.attempt)
        receipt = C.document(C.reference(self.attempt/'pause-capture.json'))
        captured = Path(receipt['captured']['path'])
        self.assertFalse(path.exists()); self.assertEqual(C.reference(captured), receipt['captured'])
        self.assertEqual(receipt['captured']['sha256'], ref['sha256'])
        self.assertEqual(captured.stat().st_ino, self.recipe['pause']['identity'][1])
        self.assertEqual(captured.parent.stat().st_mode & 0o777, 0o700)
        for directory in [captured.parent, self.run, self.attempt]:
            self.assertIn(mock.call(directory), sync.call_args_list)

    def test_cross_device_pause_rejects_preexisting_directory_and_symlink(self):
        path, ref, devices = self.cross_device_pause()
        destination = self.run/('.pause-capture-'+self.recipe['attempt_id'])
        destination.mkdir()
        with devices:
            with self.assertRaises(FileExistsError): W.remove_pause(self.recipe, self.attempt)
        self.assertEqual(C.reference(path), ref)
        destination.rmdir(); destination.symlink_to(self.root, target_is_directory=True)
        (self.attempt/'pause-before.json').unlink()  # Separate disposable test attempt.
        with devices:
            with self.assertRaises(FileExistsError): W.remove_pause(self.recipe, self.attempt)
        self.assertEqual(C.reference(path), ref); self.assertTrue(destination.is_symlink())

    def test_cross_device_pause_race_retains_both_and_mismatch_restores_without_clobber(self):
        path, ref, devices = self.cross_device_pause(); rename = os.rename
        captured = self.run/('.pause-capture-'+self.recipe['attempt_id'])/'pause-captured'
        def race(source, destination):
            rename(source, destination); path.write_bytes(b'foreign replacement')
        with devices, mock.patch.object(W.os, 'rename', side_effect=race):
            with self.assertRaisesRegex(ValueError, 'raced'): W.remove_pause(self.recipe, self.attempt)
        self.assertEqual(path.read_bytes(), b'foreign replacement')
        self.assertEqual(C.sha(captured.read_bytes()), ref['sha256'])
        self.assertFalse((self.attempt/'pause-capture.json').exists())

    def test_cross_device_pause_failed_verification_restores_original(self):
        path, ref, devices = self.cross_device_pause(); raw = C.raw
        captured = self.run/('.pause-capture-'+self.recipe['attempt_id'])/'pause-captured'
        def fail_read(p, *args):
            if p == captured: raise OSError('controlled verification read failure')
            return raw(p, *args)
        with devices, mock.patch.object(C, 'raw', side_effect=fail_read):
            with self.assertRaisesRegex(OSError, 'controlled'): W.remove_pause(self.recipe, self.attempt)
        self.assertEqual(C.reference(path), ref)
        self.assertEqual(C.reference(captured)['sha256'], ref['sha256'])
        self.assertEqual(path.stat().st_ino, captured.stat().st_ino)
        self.assertFalse((self.attempt/'pause-capture.json').exists())

    def test_failed_or_orphan_current_pointer_blocks_resume(self):
        result_path = self.control/'attempts'/'old'/'result.json'; result_path.parent.mkdir(parents=True)
        ref = self.put(result_path, {'status': 'failed_or_unknown', 'ownership_status': 'unknown_requires_review'})
        binding_ref = self.put(self.run/'binding.json', self.binding); self.recipe['binding'] = binding_ref
        review = self.put(self.root/'review.json', {'status': 'PASS', 'result': ref, 'binding': binding_ref})
        self.recipe['previous'] = {'result': ref, 'review': review}
        with self.assertRaisesRegex(ValueError, 'failed/unknown'): W.admit_previous(self.recipe, self.ctx())
        result_path.unlink()
        with self.assertRaises(FileNotFoundError): W.admit_previous(self.recipe, self.ctx())

    def test_resumed_full_drift_prevents_all_capture_and_plan_work(self):
        class Paused(Exception): pass
        called = []
        inventory = {'complete': True, 'candidates': [dict(v['candidate']) for v in self.main['outcomes']]}
        inventory['candidates'][0] = dict(inventory['candidates'][0], bytes=999)
        runner = types.SimpleNamespace(config=self.config, require=lambda key,args: called.append(args) or {'value': inventory})
        with self.assertRaisesRegex(ValueError, 'changed/incomplete'):
            W.fresh_full_admission(runner, self.recipe, self.ctx(), self.attempt, Paused)
            called.append(['capture-must-not-run'])
        self.assertEqual(called, [['discover', self.config['catalog_root']]])
        self.assertEqual(json.loads((self.attempt/'inventory-admission.json').read_bytes())['status'], 'failed_or_unresolved')
        with self.assertRaisesRegex(ValueError, 'failed/unresolved'):
            W.validate_full_admission(self.recipe, self.ctx(), self.binding, 1, False)

    def test_paths_rejects_same_count_substituted_key_and_capture_revision(self):
        value = self.full_output()
        for mutate in [lambda v: v['outcomes'][0].update(key='substituted'),
                       lambda v: v['outcomes'][0]['inspection'].update(revision='substituted')]:
            broken = json.loads(json.dumps(value)); mutate(broken)
            ref = self.put(Path(self.ctx()['output']), broken)
            recipe = dict(self.recipe, phase='paths', input=ref, baseline=ref)
            with self.assertRaises(ValueError): C.context(recipe)

    def test_normal_resume_requires_explicit_reaped_and_empty_failure_proof(self):
        ref = self.put(self.root/'previous.json', {})
        review = self.put(self.root/'previous-review.json', {'status': 'PASS', 'result': ref, 'binding': self.recipe['binding']})
        base = {'ownership_status': 'observed_owned_processes_reaped', 'exit_code': 0,
                'root_reaped': True, 'new_command_failures': []}
        for field in ['root_reaped', 'new_command_failures']:
            for missing in [True, False]:
                value = dict(base)
                if missing: value.pop(field)
                else: value[field] = None
                ref = self.put(Path(ref['path']), value)
                review = self.put(Path(review['path']), {'status': 'PASS', 'result': ref, 'binding': self.recipe['binding']})
                self.recipe['previous'] = {'result': ref, 'review': review}
                with self.assertRaisesRegex(ValueError, 'failed/unknown'): W.admit_previous(self.recipe, self.ctx())

    def test_pause_before_fresh_full_discovery_has_no_reserved_command(self):
        class Paused(Exception): pass
        def paused(*args): raise Paused()
        runner = types.SimpleNamespace(config=self.config, require=paused)
        with self.assertRaises(Paused): W.fresh_full_admission(runner, self.recipe, self.ctx(), self.attempt, Paused)
        W.validate_full_admission(self.recipe, self.ctx(), self.binding, 1, True)
        with self.assertRaisesRegex(ValueError, 'reserved'):
            W.validate_full_admission(self.recipe, self.ctx(), self.binding, 2, True)

    def imported_setup(self):
        self.attempt.rmdir()
        self.binding = {'source': 'new-source', 'binary_sha256': 'new-binary'}
        self.recipe['binding'] = self.put(self.run/'binding.json', self.binding)
        self.recipe['journal'] = C.reference(self.run/'journal.json')
        self.recipe['pause'] = {'kind': 'absent'}
        self.recipe['code'] = {'python': {'path': sys.executable}, 'runner': {'path': str(self.root/'new-runner.py')}}
        captures = []
        for i, prior in enumerate(self.main['outcomes']):
            prior['capture'] = {'state': 'captured', 'revision_id': 'r'+str(i)}
            directory = self.root/('prior-capture-'+str(i)); directory.mkdir()
            prior['capture_path'] = str(directory)
            manifest = self.put(directory/'manifest.json', {'state': 'captured', 'revision_id': 'r'+str(i),
                                                          'request': {'source': prior['candidate']['path']}})
            captures.append({'candidate_key': prior['key'], 'capture_path': str(directory), 'manifest': manifest})
        self.recipe['baseline'] = self.put(self.run/'reports/main-review.json', self.main)
        request = C.document(self.recipe['input']); request['main_review_sha256'] = self.recipe['baseline']['sha256']
        self.recipe['input'] = self.put(Path(self.recipe['input']['path']), request)
        old = self.root/'old-run'; old.mkdir(); (old/'reports').mkdir()
        old_binding = self.put(old/'binding.json', {'source': 'old-source', 'binary_sha256': 'old-binary'})
        old_main = self.put(old/'reports/main-review.json', self.main)
        old_phase = self.put(old/'reports/phase-old.json', {'phase': 'main', 'binding': C.document(old_binding),
                            'status': 'review_artifact_returned_not_acceptance', 'output': old_main['path']})
        old_result = self.put(self.root/'old-result.json', {'status': 'main_review_returned_not_acceptance',
            'exit_code': 0, 'root_reaped': True, 'ownership_status': 'observed_owned_processes_reaped',
            'new_command_failures': [], 'phase': old_phase})
        old_review = self.put(self.root/'old-review.json', {'status': 'PASS', 'result': old_result, 'binding': old_binding, 'output': old_main})
        initial_config = self.put(self.root/'initial-config.json', self.config)
        argv = [sys.executable, '-I', '-B', self.recipe['code']['runner']['path'], 'init', initial_config['path']]
        init_result = self.put(self.root/'init-result.json', {'phases': [{'argv': argv, 'exit_code': 0, 'failure': None,
            'root_reaped': True, 'ownership_status': 'observed_owned_processes_reaped'}]})
        init_review = self.put(self.root/'init-review.json', {'status': 'PASS', 'result': init_result, 'binding': self.recipe['binding']})
        proof = {'protocol': 1, 'status': 'IMPORTED_MAIN_NOT_EXECUTED', 'old_run': str(old), 'old_binding': old_binding,
                 'old_main': old_main, 'old_result': old_result, 'old_review': old_review, 'new_binding': self.recipe['binding'],
                 'new_main': self.recipe['baseline'], 'captures': captures,
                 'initialization': {'result': init_result, 'pointer': ['phases', 0], 'review': init_review, 'config': initial_config, 'argv': argv}}
        self.set_bootstrap(proof)
        return proof

    def set_bootstrap(self, proof):
        ref = self.put(self.root/'bootstrap.json', proof)
        review = self.put(self.root/'bootstrap-review.json', {'status': 'PASS', 'bootstrap': ref, 'new_binding': self.recipe['binding']})
        self.recipe['previous'] = {'kind': 'imported_main', 'bootstrap': ref, 'review': review}

    def test_imported_main_truthful_distinct_bootstrap_no_executed_main(self):
        self.imported_setup(); W.admit_previous(self.recipe, self.ctx())
        self.assertEqual(list((self.run/'commands').iterdir()), [])
        self.assertEqual(list((self.run/'reports').glob('phase-*')), [])
        self.assertEqual(C.document(self.recipe['journal']), {'next_command': 1})

    def test_imported_main_wrong_binding_copy_manifest_or_init_cannot_enter_full(self):
        proof = self.imported_setup(); original = json.loads(json.dumps(proof))
        changes = [lambda p: p.update(new_binding=p['old_binding']),
                   lambda p: p.update(old_binding=self.recipe['binding']),
                   lambda p: p['captures'][0]['manifest'].update(sha256='0'*64),
                   lambda p: p['initialization'].update(argv=['fabricated-init']),
                   lambda p: p['captures'].pop()]
        for change in changes:
            with self.subTest(change=change):
                value = json.loads(json.dumps(original)); change(value); self.set_bootstrap(value)
                with self.assertRaises(ValueError): W.admit_previous(self.recipe, self.ctx())
        self.set_bootstrap(original)
        old_main = C.document(proof['old_main']); old_main['candidate_count'] = 999
        proof['old_main'] = self.put(Path(proof['old_main']['path']), old_main); self.set_bootstrap(proof)
        with self.assertRaisesRegex(ValueError, 'copy bytes'): W.admit_previous(self.recipe, self.ctx())

    def test_imported_main_current_or_orphan_attempt_and_reserved_command_rejected(self):
        self.imported_setup()
        self.put(self.control/'current.json', {'attempt_id': 'foreign'})
        with self.assertRaisesRegex(ValueError, 'preexisting'): W.admit_previous(self.recipe, self.ctx())
        (self.control/'current.json').unlink(); self.attempt.mkdir()
        with self.assertRaisesRegex(ValueError, 'preexisting'): W.admit_previous(self.recipe, self.ctx())
        self.attempt.rmdir(); (self.run/'commands/000000001').mkdir()
        with self.assertRaisesRegex(ValueError, 'empty commands'): W.admit_previous(self.recipe, self.ctx())

    def legacy_owner_fixture(self):
        proof = self.imported_setup()
        source = self.root/'legacy-owner.py'
        constants = {'CORE': 'old-source', 'BINARY_SHA': 'old-binary', 'RUNNER_SHA': 'old-runner',
                     'HELPER_SHA': 'old-helper', 'CONFIG_SHA': 'old-config', 'PYTHON': sys.executable,
                     'SUPERVISOR_SHA': 'old-supervisor'}
        source.write_text('\n'.join(k+' = '+repr(v) for k,v in constants.items())+'\n')
        wrapper = C.reference(source)
        binding = {'source': 'old-source', 'binary_sha256': 'old-binary', 'driver_sha256': 'old-runner',
                   'generation_driver_sha256': 'old-helper', 'config_sha256': 'old-config'}
        proof['old_binding'] = self.put(Path(proof['old_binding']['path']), binding)
        control = self.root/'legacy-control'; attempt = control/'attempts'/'old-attempt'; attempt.mkdir(parents=True)
        funding = self.put(self.root/'old-funding.json', {'binding': proof['old_binding'], 'run': proof['old_run'],
                           'attempt': str(attempt), 'adapter': {'path': str(self.root/'old-funding.py')}})
        recipe = self.put(self.root/'old-recipe.json', {'binding': proof['old_binding'], 'run': proof['old_run'],
                         'control': str(control), 'attempt_id': 'old-attempt', 'funding_policy': funding})
        started = self.put(attempt/'started.json', {'recipe': recipe, 'wrapper_sha256': wrapper['sha256'],
                           'supervisor_sha256': constants['SUPERVISOR_SHA'], 'started_unix': 1})
        process = self.put(attempt/'process.json', {'pid': 12345, 'started_unix': 2,
            'argv': [sys.executable, str(self.root/'old-funding.py'), funding['path'], funding['sha256']]})
        old = C.document(proof['old_result']); old.pop('root_reaped')
        phase = C.document(old['phase']); phase['binding'] = binding
        old['phase'] = self.put(Path(old['phase']['path']), phase)
        old.update(cleanup=None, funding_policy=funding, started_unix=2, finished_unix=3)
        proof['old_result'] = self.put(attempt/'result.json', old)
        proof['old_review'] = self.put(Path(proof['old_review']['path']), {'status': 'PASS', 'result': proof['old_result'],
                                      'binding': proof['old_binding'], 'output': proof['old_main']})
        # Real launches redirect stdout/stderr; terminal tool poll output is empty.
        terminal = self.put(self.root/'actual-tool-wait.json', {'exit_code': 0, 'output': ''})
        owner = {'kind': 'legacy_main_v3', 'wrapper': wrapper, 'started': started, 'process': process,
                 'recipe': recipe, 'outer_wait': terminal}
        owner['review'] = self.put(self.root/'outer-review.json', dict(owner, status='PASS', kind='legacy_main_v3_outer_wait',
                                  result=proof['old_result'], binding=proof['old_binding'], session_id=99))
        proof['old_owner'] = owner; self.set_bootstrap(proof)
        return proof, wrapper['sha256']

    def test_legacy_missing_root_field_only_with_pinned_reap_and_native_shape_provenance(self):
        proof, pinned = self.legacy_owner_fixture()
        self.assertNotIn('root_reaped', C.document(proof['old_result']))
        with mock.patch.object(W, 'LEGACY_MAIN_V3_SHA', pinned):
            W.admit_previous(self.recipe, self.ctx())
            without = dict(proof); without.pop('old_owner'); self.set_bootstrap(without)
            with self.assertRaisesRegex(ValueError, 'reaping'): W.admit_previous(self.recipe, self.ctx())
            self.set_bootstrap(proof)
        with self.assertRaisesRegex(ValueError, 'unsupported'): W.admit_previous(self.recipe, self.ctx())

    def test_legacy_wait_wrong_session_proof_process_and_paused_output_rejected(self):
        proof, pinned = self.legacy_owner_fixture(); owner = proof['old_owner']
        with mock.patch.object(W, 'LEGACY_MAIN_V3_SHA', pinned):
            for name, field, value in [('process', 'pid', None), ('process', 'argv', ['wrong']),
                                        ('started', 'wrapper_sha256', 'foreign'),
                                        ('outer_wait', 'exit_code', 1), ('outer_wait', 'session_id', 99),
                                        ('review', 'session_id', None)]:
                original = C.document(owner[name]); broken = dict(original, **{field: value})
                ref = owner[name]; owner[name] = self.put(Path(ref['path']), broken); self.set_bootstrap(proof)
                with self.assertRaises(ValueError): W.admit_previous(self.recipe, self.ctx())
                owner[name] = self.put(Path(ref['path']), original)
            original = C.document(proof['old_result']); paused = dict(original, status='paused_at_command_boundary', exit_code=1)
            proof['old_result'] = self.put(Path(proof['old_result']['path']), paused)
            proof['old_review'] = self.put(Path(proof['old_review']['path']), {'status':'PASS','result':proof['old_result'],
                'binding':proof['old_binding'],'output':proof['old_main']})
            self.set_bootstrap(proof)
            with self.assertRaisesRegex(ValueError, 'terminal'): W.admit_previous(self.recipe, self.ctx())

    def test_helper_bytes_checked_before_execution(self):
        marker = self.root/'must-not-exist'; code = self.root/'wrong.py'
        code.write_text('from pathlib import Path\nPath('+repr(str(marker))+').touch()\n')
        with self.assertRaisesRegex(ValueError, 'hash'): W.module({'path': str(code), 'sha256': '0'*64}, 'untrusted')
        self.assertFalse(marker.exists())

    def temp_storage(self):
        directory = self.root/'sqlite-temp'; directory.mkdir()
        meta = directory.stat()
        storage = {'directory': str(directory), 'device': meta.st_dev, 'inode': meta.st_ino,
                   'environment': {k: str(directory) for k in ['SQLITE_TMPDIR', 'TMPDIR']}}
        self.recipe['temp_storage'] = storage
        return directory, storage

    def test_temp_storage_optional_exact_environment_and_changing_contents(self):
        W.validate_temp_storage(self.recipe)  # Existing recipes remain compatible.
        directory, storage = self.temp_storage()
        with mock.patch.dict(os.environ, storage['environment']):
            W.validate_temp_storage(self.recipe)
            (directory/'ordinary-temp-file').write_bytes(b'temp')
            W.validate_temp_storage(self.recipe)  # Directory mtime is not identity.

    def test_temp_storage_unset_mismatched_and_unbound_environment_rejected(self):
        directory, storage = self.temp_storage()
        for key in storage['environment']:
            for value in [None, str(self.root)]:
                with self.subTest(key=key, value=value), mock.patch.dict(os.environ, storage['environment']):
                    if value is None: os.environ.pop(key, None)
                    else: os.environ[key] = value
                    with self.assertRaisesRegex(ValueError, 'environment'): W.validate_temp_storage(self.recipe)
        with mock.patch.dict(os.environ, storage['environment']):
            storage['environment'] = {'SQLITE_TMPDIR': str(directory)}
            with self.assertRaisesRegex(ValueError, 'environment'): W.validate_temp_storage(self.recipe)

    def test_temp_storage_replacement_missing_symlink_and_special_rejected(self):
        directory, storage = self.temp_storage()
        retained = self.root/'retained-temp'
        with mock.patch.dict(os.environ, storage['environment']):
            directory.rename(retained)  # Retains the original inode, avoiding reuse.
            with self.assertRaises(FileNotFoundError): W.validate_temp_storage(self.recipe)
            directory.mkdir()
            with self.assertRaisesRegex(ValueError, 'identity'): W.validate_temp_storage(self.recipe)
            directory.rmdir(); directory.symlink_to(retained, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, 'symlink'): W.validate_temp_storage(self.recipe)
            directory.unlink(); directory.write_bytes(b'not a directory')
            storage['inode'] = directory.stat().st_ino
            with self.assertRaisesRegex(ValueError, 'type'): W.validate_temp_storage(self.recipe)

    def test_temp_storage_device_and_access_checks(self):
        directory, storage = self.temp_storage()
        with mock.patch.dict(os.environ, storage['environment']):
            storage['device'] += 1
            with self.assertRaisesRegex(ValueError, 'identity'): W.validate_temp_storage(self.recipe)
            storage['device'] -= 1
            with mock.patch.object(W.os, 'access', return_value=False):
                with self.assertRaisesRegex(ValueError, 'access'): W.validate_temp_storage(self.recipe)
            actual_stat = Path.stat
            def different_run_device(path, *args, **kwargs):
                if path == self.run: return types.SimpleNamespace(st_dev=storage['device']+1)
                return actual_stat(path, *args, **kwargs)
            with mock.patch.object(Path, 'stat', different_run_device):
                with self.assertRaisesRegex(ValueError, 'share a device'): W.validate_temp_storage(self.recipe)

    def test_temp_storage_command_and_space_check_before_frozen_work(self):
        directory, storage = self.temp_storage(); touched = []
        class Runner:
            def __init__(self, root): pass
            def call(self, *args): touched.append('call')
            def space(self, *args): touched.append('space')
        frozen = types.SimpleNamespace(Runner=Runner, PauseRequested=RuntimeError)
        monitor = types.SimpleNamespace(finish=lambda: None)
        guard = types.SimpleNamespace(FundingMonitor=lambda *args: monitor, guarded_type=lambda base, *args: base)
        def main():
            runner = frozen.Runner(self.run)
            os.environ.pop('SQLITE_TMPDIR')
            for operation in [lambda: runner.call(['fixture'], ['rows']), runner.space]:
                with self.assertRaisesRegex(ValueError, 'environment'): operation()
        frozen.main = main
        self.recipe['code'] = {'runner': {'path': 'fixture-runner'}, 'funding_guard': {}}
        self.recipe['journal'] = C.reference(self.run/'journal.json')
        self.put(self.control/'current.json', {'attempt_id': self.recipe['attempt_id']})
        self.put(self.attempt/'started.json', {'binding': self.recipe['binding']})
        with mock.patch.dict(os.environ, storage['environment']), \
                mock.patch.object(W, 'validate_recipe', return_value=(self.ctx(), {}, self.binding, self.config)), \
                mock.patch.object(W, 'module', side_effect=[frozen, guard]):
            W.child_main(self.recipe)
        self.assertEqual(touched, [])
        self.assertEqual(C.document(self.recipe['journal']), {'next_command': 1})

    def test_temp_storage_outer_replacement_reaps_actual_child(self):
        directory, storage = self.temp_storage()
        def replace_after_launch(*args):
            directory.rename(self.root/'retained-temp'); directory.mkdir()
            return {}
        with mock.patch.dict(os.environ, storage['environment']):
            value = self.real_child_failure(replace_after_launch)
        self.assertIn('temp_storage directory identity', value['failure'])

    def test_real_child_output_is_capped_and_root_reaped(self):
        child = subprocess.Popen([sys.executable, '-I', '-B', '-c', 'import sys;sys.stdout.buffer.write(b"x"*200000)'], stdout=subprocess.PIPE)
        try:
            with mock.patch.object(W, 'LOG_CAP', 65536):
                drain = W.Drain(child.stdout, self.root/'stdout'); drain.thread.start()
                self.assertEqual(child.wait(timeout=5), 0); drain.thread.join(timeout=5)
            self.assertFalse(drain.thread.is_alive()); self.assertTrue(drain.excess.is_set())
            self.assertEqual((self.root/'stdout').stat().st_size, 65536); self.assertEqual(drain.seen, 200000)
        finally:
            if child.poll() is None: child.kill()
            child.wait(timeout=5)

    def real_child_failure(self, failure):
        script = self.root/'sleep.py'; script.write_text('import time;time.sleep(30)\n')
        self.recipe['code'] = {'python': {'path': sys.executable}, 'controller': {'path': str(script)}}
        self.recipe['memory'] = {k: 256*C.MIB for k in ['python_process_rss_bytes', 'native_process_rss_bytes', 'combined_owned_rss_bytes']}
        attempt = self.root/'attempt'; attempt.mkdir(); self.put(attempt/'recipe.json', {})
        owned = []
        def stop(child, grace, outcome):
            if child.poll() is None: child.kill()
            child.wait(timeout=5); owned.append(child); outcome['root_reaped'] = True
        def halt(child, *args):
            value = {}; stop(child, None, value); return value
        supervisor = types.SimpleNamespace(observe=mock.Mock(side_effect=failure), halt=halt, stop_root=stop)
        monitor = types.SimpleNamespace(outer=lambda: None, finish=lambda: None)
        guard = types.SimpleNamespace(FundingMonitor=lambda *args: monitor)
        value = W.supervise(self.recipe, attempt, self.ctx(), {}, self.binding, supervisor, guard)
        self.assertEqual(value['status'], 'failed_or_unknown'); self.assertEqual(value['ownership_status'], 'unknown_requires_review')
        self.assertTrue(value['root_reaped']); self.assertEqual(len(owned), 1); self.assertIsNotNone(owned[0].returncode)
        return value

    def test_real_child_observer_failure_still_stops_and_reaps(self):
        self.real_child_failure(PermissionError('controlled observer failure'))

    def test_actual_sigterm_owner_still_stops_and_reaps_separate_child(self):
        prior = signal.getsignal(signal.SIGTERM)
        def terminate(*args): signal.raise_signal(signal.SIGTERM)
        self.real_child_failure(terminate)
        self.assertEqual(signal.getsignal(signal.SIGTERM), prior)


    def canonical_setup(self):
        import ast
        # Extract the repository candidate's two actual pure definitions, not a
        # mocked optimized encoder. No driver module or native binary executes.
        source = (ROOT/'scripts/run_lightroom_inspection.py').read_text()
        tree = ast.parse(source)
        nodes = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in W.CANONICAL_FUNCTIONS]
        helper = self.root/'canonical.py'
        helper.write_text('import json\n'+ '\n'.join(ast.get_source_segment(source,n) for n in nodes)+'\n')
        base = self.root/'base.py'; base.write_text('def unchanged_base(): pass\n')
        runtime = self.root/'runtime'; runtime.write_text('synthetic runtime identity; never executed\n')
        controller = self.root/'controller.py'; controller.write_text('synthetic controller identity\n')
        self.recipe['code'] = {'runner':C.reference(base),'python':C.reference(runtime),'controller':C.reference(controller)}
        review = {'status':'PASS','author':'synthetic test reviewer', 'base_driver':C.reference(base),
                  'helper':C.reference(helper),'runtime':C.reference(runtime),
                  'assertions':{'canonical_bytes_equal':True,'bounded_fast_path':True,'fallback_preserved':True}}
        profile = {'protocol':1,'kind':'canonical_hash_override','base_driver':C.reference(base),'helper':C.reference(helper),
                   'runtime':C.reference(runtime),'functions':W.CANONICAL_FUNCTIONS,
                   'equivalence_review':self.put(self.root/'equivalence.json',review)}
        self.recipe['canonical_hash_profile'] = self.put(self.root/'profile.json',profile)
        return profile

    def canonical_previous(self):
        self.canonical_setup()
        old_id = '00000000-0000-4000-8000-000000000002'
        old_attempt = self.control/'attempts'/old_id; old_attempt.mkdir()
        old_recipe = json.loads(json.dumps(self.recipe)); old_recipe.pop('canonical_hash_profile')
        old_recipe['attempt_id'] = old_id; old_recipe['code']['controller']['sha256'] = W.LEGACY_FULL_CONTROLLER_SHA
        old_recipe_ref = self.put(old_attempt/'recipe.json',old_recipe)
        self.recipe['journal'] = C.reference(self.run/'journal.json')
        phase = self.put(self.run/'reports/phase-old.json', {'phase':'full','binding':self.binding,'status':'paused'})
        old = {'status':'paused_at_command_boundary','ownership_status':'observed_owned_processes_reaped','root_reaped':True,
               'new_command_failures':[],'exit_code':1,'cleanup':None,'journal':self.recipe['journal'],'next_command':1,
               'phase':phase,'phase_name':'full','phase_input':self.recipe['input']}
        old_ref = self.put(old_attempt/'result.json',old)
        review = {'status':'PASS','result':old_ref,'binding':self.recipe['binding']}
        review_ref = self.put(self.root/'old-review.json',review)
        self.recipe['previous'] = {'result':old_ref,'review':review_ref}
        self.put(self.control/'current.json',{'attempt_id':old_id})
        transition = {'status':'PASS','kind':'legacy_full_pause_to_canonical_hash_profile','profile':self.recipe['canonical_hash_profile'],
                      'previous':self.recipe['previous'],'previous_recipe':old_recipe_ref,'base_binding':self.recipe['binding'],
                      'journal':self.recipe['journal'],'expected_next_command':1,
                      'invariants_sha256':C.sha(C.encoded(W.canonical_invariants(self.recipe)))}
        self.recipe['canonical_hash_transition'] = self.put(self.root/'transition.json',transition)
        return old,review,old_attempt

    def test_canonical_transition_and_real_encoder_replay_preserve_bytes(self):
        import hashlib
        self.canonical_previous(); W.admit_previous(self.recipe,self.ctx())
        self.put(self.attempt/'recipe.json',self.recipe)
        W.save(self.attempt/'execution-profile.json',W.execution_profile_value(self.recipe,self.attempt))
        frozen = types.SimpleNamespace(encoded=lambda x: json.dumps(x,sort_keys=True,separators=(',',':'),ensure_ascii=True).encode()+b'\n')
        original_encoded = frozen.encoded
        self.put(self.attempt/'process.json',{'pid':os.getpid()})
        W.install_canonical_profile(self.recipe,self.attempt,frozen)
        self.assertIs(frozen.encoded,original_encoded)
        values = [{'z':[1,-0.0,None,True], 'unicode':'é😀\ud800'}, {'large':'x'*70000}, [1,2,3]]
        old_digest = hashlib.sha256(); new_digest = hashlib.sha256()
        for value in values:
            old_digest.update(original_encoded(value)); frozen.update_canonical_hash(new_digest,value)
        self.assertEqual(old_digest.digest(),new_digest.digest())
        fields = W.canonical_result_fields(self.recipe)
        self.assertEqual(C.document(fields['execution_profile'])['first_command'],1)
        self.assertEqual(C.document(fields['execution_profile_consumed'])['pid'],os.getpid())
        # Execute the actual current Runner.call replay branch with a completed
        # synthetic native receipt. Any fresh launch/reservation is an error.
        driver = load('canonical_replay_fixture','run_lightroom_inspection.py')
        key = ['full',self.ctx()['id'],'capture']; args=['capture','never-read','@CAPTURE@']
        record = self.record(args,key)
        stdout = self.put(self.run/'commands/000000001/stdout',{'state':'captured','revision_id':'fixed'})
        record['stdout']={'path':'commands/000000001/stdout','sha256':stdout['sha256']}
        record['stdout_cap']=1024
        self.put(self.run/'commands/000000001/result.json',record)
        runner=driver.Runner.__new__(driver.Runner); runner.root=self.run
        before = (self.run/'journal.json').read_bytes()
        with mock.patch.object(driver.subprocess,'Popen',side_effect=AssertionError('native replayed twice')):
            result=runner.call(key,args)
        self.assertTrue(result['ok']); self.assertEqual(result['value']['revision_id'],'fixed')
        self.assertEqual((self.run/'journal.json').read_bytes(),before)

    def test_canonical_wrong_equivalence_source_and_top_level_execution_rejected(self):
        profile=self.canonical_setup(); W.canonical_hash_profile(self.recipe)
        original=json.loads(json.dumps(profile)); review=C.document(profile['equivalence_review'])
        review['assertions']['canonical_bytes_equal']=False
        profile['equivalence_review']=self.put(self.root/'bad-review.json',review)
        self.recipe['canonical_hash_profile']=self.put(self.root/'bad-profile.json',profile)
        with self.assertRaisesRegex(ValueError,'review'): W.canonical_hash_profile(self.recipe)
        self.recipe['canonical_hash_profile']=self.put(self.root/'profile.json',original)
        Path(original['helper']['path']).write_text('raise RuntimeError("must never execute")\n')
        with self.assertRaisesRegex(ValueError,'hash'): W.canonical_hash_profile(self.recipe)
        for source in ['raise RuntimeError("never")', 'def update_canonical_hash(d,v=print("never")): pass',
                       'import os\ndef update_canonical_hash(d,v): pass',
                       'def update_canonical_hash(d,v)->print("never"): pass']:
            with self.subTest(source=source), self.assertRaises(ValueError): W.canonical_functions(source,'synthetic')

    def test_canonical_transition_rejects_scope_drift_missing_proof_and_failed_orphan(self):
        self.canonical_previous()
        original=json.loads(json.dumps(self.recipe))
        for mutate in [lambda r:r.pop('canonical_hash_transition'),lambda r:r.update(funding={'changed':True}),
                       lambda r:r.update(temp_storage={'changed':True}),lambda r:r.update(expected_next_command=2)]:
            self.recipe=json.loads(json.dumps(original));mutate(self.recipe)
            with self.assertRaises((ValueError,KeyError)):W.admit_previous(self.recipe,self.ctx())
        self.recipe=original
        self.put(self.control/'current.json',{'attempt_id':'orphan'})
        with self.assertRaisesRegex(ValueError,'bypassed'):W.admit_previous(self.recipe,self.ctx())
        old=C.document(self.recipe['previous']['result']);old['new_command_failures']=[1]
        self.recipe['previous']['result']=self.put(Path(self.recipe['previous']['result']['path']),old)
        review=C.document(self.recipe['previous']['review']);review['result']=self.recipe['previous']['result']
        self.recipe['previous']['review']=self.put(Path(self.recipe['previous']['review']['path']),review)
        with self.assertRaisesRegex(ValueError,'failed/unknown'):W.admit_previous(self.recipe,self.ctx())

    def test_canonical_same_profile_continuation_and_downgrade_rejected(self):
        old,review,old_attempt=self.canonical_previous()
        old_recipe=json.loads(json.dumps(self.recipe));old_recipe['attempt_id']=old_attempt.name
        self.put(old_attempt/'recipe.json',old_recipe)
        self.put(old_attempt/'process.json',{'pid':os.getpid()})
        W.save(old_attempt/'execution-profile.json',W.execution_profile_value(old_recipe,old_attempt))
        W.install_canonical_profile(old_recipe,old_attempt,types.SimpleNamespace())
        old.update(execution_profile=C.reference(old_attempt/'execution-profile.json'),
                   execution_profile_consumed=C.reference(old_attempt/'execution-profile-consumed.json'))
        self.recipe['previous']['result']=self.put(old_attempt/'result.json',old)
        review.update(result=self.recipe['previous']['result'],execution_profile=old['execution_profile'])
        self.recipe['previous']['review']=self.put(self.root/'new-review.json',review)
        self.recipe.pop('canonical_hash_transition')
        W.admit_previous(self.recipe,self.ctx())
        self.recipe.pop('canonical_hash_profile')
        with self.assertRaisesRegex(ValueError,'removed'):W.admit_previous(self.recipe,self.ctx())

    def test_canonical_dispatch_requires_durable_authority_and_consumed_identity(self):
        self.canonical_previous();self.put(self.attempt/'recipe.json',self.recipe)
        with self.assertRaises(FileNotFoundError): W.install_canonical_profile(self.recipe,self.attempt,types.SimpleNamespace())
        W.save(self.attempt/'execution-profile.json',W.execution_profile_value(self.recipe,self.attempt))
        W.install_canonical_profile(self.recipe,self.attempt,types.SimpleNamespace())
        self.put(self.attempt/'process.json',{'pid':os.getpid()+1})
        with self.assertRaisesRegex(ValueError,'consumption'):W.canonical_result_fields(self.recipe)
        self.put(self.attempt/'process.json',{'pid':os.getpid()})
        with self.assertRaises(FileExistsError):W.install_canonical_profile(self.recipe,self.attempt,types.SimpleNamespace())


    def test_canonical_same_profile_full_paths_packets_and_changed_profile_rejected(self):
        self.canonical_previous()
        # Construct two successful, explicitly profiled synthetic predecessors.
        # The ordinary output/phase checks are exercised, not patched away.
        for next_phase in ['paths','packets']:
            current = json.loads(json.dumps(self.recipe)); old_attempt=self.attempt
            current['attempt_id']=old_attempt.name
            self.put(old_attempt/'recipe.json',current)
            self.put(old_attempt/'process.json',{'pid':os.getpid()})
            W.save(old_attempt/'execution-profile.json',W.execution_profile_value(current,old_attempt))
            W.install_canonical_profile(current,old_attempt,types.SimpleNamespace())
            ctx=self.ctx()
            if current['phase']=='full':
                output=self.put(Path(ctx['output']),self.full_output())
            else:
                value=C.document(self.recipe['input']);output=self.put(Path(ctx['output']),self.path_output(value,self.recipe['input']))
            phase=self.put(self.run/'reports'/('phase-'+current['phase']+'.json'),
                           {'phase':current['phase'],'status':'review_artifact_returned_not_acceptance','output':output['path'],'binding':self.binding})
            prior={'status':'review_returned_not_acceptance','phase':phase,'phase_name':current['phase'],'phase_input':current['input'],
                   'output':output,'journal':self.recipe['journal'],'next_command':1,'exit_code':0,'root_reaped':True,
                   'ownership_status':'observed_owned_processes_reaped','new_command_failures':[],
                   'execution_profile':C.reference(old_attempt/'execution-profile.json'),
                   'execution_profile_consumed':C.reference(old_attempt/'execution-profile-consumed.json')}
            prior_ref=self.put(old_attempt/'result.json',prior)
            review_ref=self.put(self.root/('review-'+current['phase']+'.json'),
                                {'status':'PASS','result':prior_ref,'binding':self.recipe['binding'],'output':output,'execution_profile':prior['execution_profile']})
            self.put(self.control/'current.json',{'attempt_id':old_attempt.name})
            self.recipe.pop('canonical_hash_transition',None)
            self.recipe.update(phase=next_phase,previous={'result':prior_ref,'review':review_ref},attempt_id=str(__import__('uuid').uuid4()))
            if next_phase=='paths':self.recipe.update(input=output,baseline=output)
            else:self.recipe['paths_review']=output
            W.admit_previous(self.recipe,self.ctx())
            original=self.recipe['canonical_hash_profile']
            self.recipe['canonical_hash_profile']=self.put(self.root/('different-'+next_phase+'.json'),C.document(original))
            with self.assertRaisesRegex(ValueError,'profile/controller differs'):W.admit_previous(self.recipe,self.ctx())
            self.recipe['canonical_hash_profile']=original
            self.attempt=self.control/'attempts'/self.recipe['attempt_id'];self.attempt.mkdir()


    def replay_profile_setup(self):
        import ast
        profile = PhaseContracts.canonical_setup(self)
        source = (ROOT/'scripts/run_lightroom_inspection.py').read_text()
        node = next(n for n in ast.parse(source).body if isinstance(n, ast.FunctionDef) and n.name == 'read_json')
        # The standalone reviewed helper uses the same concrete default without
        # importing the driver's module globals or executing any module code.
        node.args.defaults = [ast.Constant(16*W.MIB)]
        class StandaloneConstants(ast.NodeTransformer):
            def visit_Name(self, value):
                if value.id == 'MIB':
                    return ast.copy_location(ast.Constant(W.MIB), value)
                return value
        node = StandaloneConstants().visit(node)
        helper = Path(profile['helper']['path'])
        helper.write_text(helper.read_text()+ast.unparse(node)+'\n')
        profile.update(protocol=2, kind='canonical_hash_and_replay_json_override',
                       functions=W.REPLAY_FUNCTIONS, helper=C.reference(helper))
        review = C.document(profile['equivalence_review'])
        review['helper'] = profile['helper']
        review['assertions'].update(read_json_equivalent=True, raw_bytes_released_before_parse=True)
        profile['equivalence_review'] = self.put(self.root/'replay-equivalence.json', review)
        self.recipe['canonical_hash_profile'] = self.put(self.root/'replay-profile.json', profile)
        return profile

    def test_replay_profile_dispatch_binds_reader_without_changing_base_encoding(self):
        import hashlib
        self.replay_profile_setup()
        self.recipe.update(previous={'synthetic_unexecuted_predecessor':True},
                           journal=C.reference(self.run/'journal.json'))
        self.put(self.attempt/'recipe.json', self.recipe)
        self.put(self.attempt/'process.json', {'pid':os.getpid()})
        W.save(self.attempt/'execution-profile.json', W.execution_profile_value(self.recipe,self.attempt))
        frozen = types.SimpleNamespace(encoded=lambda x: json.dumps(x,sort_keys=True,separators=(',',':'),ensure_ascii=True).encode()+b'\n', read_json=None)
        original = frozen.encoded
        W.install_canonical_profile(self.recipe,self.attempt,frozen)
        self.assertIs(frozen.encoded, original)
        path = self.root/'utf16.json'; path.write_bytes('{"key":"é"}'.encode('utf-16'))
        self.assertEqual(frozen.read_json(path), {'key':'é'})
        with self.assertRaisesRegex(ValueError,'JSON admission'): frozen.read_json(path,1)
        data = {'nested':[1,-0.0,'\ud800']}; digest = hashlib.sha256()
        frozen.update_canonical_hash(digest,data)
        self.assertEqual(digest.digest(),hashlib.sha256(original(data)).digest())
        consumed = C.document(W.canonical_result_fields(self.recipe)['execution_profile_consumed'])
        self.assertEqual(consumed['pid'],os.getpid())
        self.assertEqual(consumed['helper'],C.document(self.recipe['canonical_hash_profile'])['helper'])

    def test_replay_profile_requires_reader_equivalence_and_complete_function_roster(self):
        profile = self.replay_profile_setup(); original = json.loads(json.dumps(profile))
        for key in ['read_json_equivalent','raw_bytes_released_before_parse']:
            review = C.document(original['equivalence_review']); review['assertions'].pop(key)
            profile = dict(original,equivalence_review=self.put(self.root/('missing-'+key+'.json'),review))
            self.recipe['canonical_hash_profile'] = self.put(self.root/'invalid-profile.json',profile)
            with self.assertRaisesRegex(ValueError,'review'): W.canonical_hash_profile(self.recipe)
        profile = dict(original,functions=W.CANONICAL_FUNCTIONS)
        self.recipe['canonical_hash_profile'] = self.put(self.root/'incomplete-profile.json',profile)
        with self.assertRaisesRegex(ValueError,'identity'): W.canonical_hash_profile(self.recipe)
        source = Path(original['helper']['path']).read_text()
        with self.assertRaisesRegex(ValueError,'unapproved module'):
            W.canonical_functions(source+'\nraise RuntimeError("must not execute")\n','synthetic',W.REPLAY_FUNCTIONS)
        with self.assertRaisesRegex(ValueError,'unapproved module'):
            W.canonical_functions(source,'synthetic',W.CANONICAL_FUNCTIONS)

    def test_replay_profile_same_profile_full_paths_packets_remains_guarded(self):
        # Same-profile continuation is reusable. A private corrective execution
        # owner is not made into a public failed-predecessor admission route.
        with mock.patch.object(self,'canonical_setup',side_effect=self.replay_profile_setup):
            self.test_canonical_same_profile_full_paths_packets_and_changed_profile_rejected()

    def test_replay_profile_does_not_authorize_legacy_transition_or_failed_retry(self):
        with mock.patch.object(self,'canonical_setup',side_effect=self.replay_profile_setup):
            self.canonical_previous()
        with self.assertRaisesRegex(ValueError,'legacy transition admits only protocol1'):
            W.admit_previous(self.recipe,self.ctx())
        old = C.document(self.recipe['previous']['result']); old.update(status='failed_or_unknown',ownership_status='unknown_requires_review')
        self.recipe['previous']['result'] = self.put(Path(self.recipe['previous']['result']['path']),old)
        review = C.document(self.recipe['previous']['review']);review['result']=self.recipe['previous']['result']
        self.recipe['previous']['review'] = self.put(Path(self.recipe['previous']['review']['path']),review)
        with self.assertRaisesRegex(ValueError,'failed/unknown'): W.admit_previous(self.recipe,self.ctx())


if __name__ == '__main__': unittest.main()
