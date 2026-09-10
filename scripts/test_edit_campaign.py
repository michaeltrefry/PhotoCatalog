"""Source-contract tests, no workload launch or source-photo access."""
import copy
import hashlib
import json
import errno
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
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

class ActualChildSupervisorContracts(unittest.TestCase):
    """Tiny real children, no database, renderer, source image, or campaign."""
    def invoke(self,root,code,*,expect_failure,patches=()):
        spawned=[]
        popen=subprocess.Popen
        def launch(*args,**kwargs):
            child=popen(*args,**kwargs)
            spawned.append(child)
            return child
        from contextlib import ExitStack
        folder=Path(root)/'attempt'
        start=time.monotonic()
        try:
            with ExitStack() as stack:
                stack.enter_context(patch.object(campaign.subprocess,'Popen',side_effect=launch))
                for mocked in patches:
                    stack.enter_context(mocked)
                if expect_failure:
                    with self.assertRaisesRegex(RuntimeError,'child failed; retained at'):
                        campaign.invoke([sys.executable,'-c',code],folder,
                            dict(deadline_seconds=10,process_rss_bytes=256*campaign.MIB,
                                 group_rss_bytes=512*campaign.MIB,free_reserve_bytes=0),root)
                else:
                    campaign.invoke([sys.executable,'-c',code],folder,
                        dict(deadline_seconds=10,process_rss_bytes=256*campaign.MIB,
                             group_rss_bytes=512*campaign.MIB,free_reserve_bytes=0),root)
            self.assertEqual(len(spawned),1)
            self.assertIsNotNone(spawned[0].returncode,'supervisor did not reap its Popen child')
            self.assertLess(time.monotonic()-start,15,'bounded cleanup exceeded test allowance')
            result=json.loads((folder/'result.json').read_text())
            self.assertEqual(result['complete'],not expect_failure)
            self.assertTrue(result['ownership']['root_reaped'])
            return result,folder
        finally:
            # Test ownership is independent of the implementation under test;
            # a failing old implementation cannot leak the sleeping child.
            for child in spawned:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=5)

    def test_initial_process_or_identity_inspection_failure_still_reaps_root(self):
        for attribute in ('process','identity'):
            with self.subTest(failure=attribute), tempfile.TemporaryDirectory() as root:
                target=patch.object(campaign.psutil,'Process',side_effect=campaign.psutil.AccessDenied(pid=42)) if attribute=='process' else \
                    patch.object(campaign,'identity',side_effect=campaign.psutil.AccessDenied(pid=42))
                result,_=self.invoke(root,'import time; time.sleep(30)',expect_failure=True,patches=(target,))
                self.assertIn('AccessDenied',result['error'])
                self.assertIsNotNone(result['ownership']['root_returncode'])

    def test_inspection_failure_during_cleanup_cannot_skip_root_kill_and_wait(self):
        with tempfile.TemporaryDirectory() as root:
            result,_=self.invoke(root,'import time; time.sleep(30)',expect_failure=True,patches=(
                patch.object(campaign,'same_alive',side_effect=campaign.psutil.AccessDenied(pid=42)),
                patch.object(campaign,'owned_process',side_effect=campaign.psutil.AccessDenied(pid=42)),))
            self.assertIn('AccessDenied',result['error'])
            self.assertFalse(result['ownership']['known_absent'],'unreadable identities must remain unknown')
            self.assertTrue(any('AccessDenied' in value for value in result['ownership']['errors']))

    def test_owned_root_ignoring_term_reaches_kill_and_reap(self):
        with tempfile.TemporaryDirectory() as root:
            ready=Path(root)/'ready'
            code='import os,signal,time,pathlib; '+ \
                '(signal.signal(signal.SIGTERM,signal.SIG_IGN) if os.name=="posix" else None); '+ \
                'pathlib.Path('+repr(str(ready))+').write_text("ready"); time.sleep(30)'
            child=subprocess.Popen([sys.executable,'-c',code])
            try:
                deadline=time.monotonic()+5
                while not ready.exists() and child.poll() is None and time.monotonic()<deadline:
                    time.sleep(.01)
                self.assertTrue(ready.exists(),'child did not establish the fault precondition')
                started=time.monotonic()
                proof=campaign.terminate_owned(child,{})
                self.assertTrue(proof['root_reaped'])
                self.assertLess(time.monotonic()-started,12)
                if os.name=='posix':
                    self.assertEqual(child.returncode,-signal.SIGKILL)
            finally:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=5)

    def test_reused_identity_never_signals_unrelated_actual_child(self):
        root=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])
        foreign=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])
        try:
            foreign_key=campaign.identity(campaign.psutil.Process(foreign.pid))
            stale=(foreign_key[0],foreign_key[1]-1)
            proof=campaign.terminate_owned(root,{stale:True})
            self.assertTrue(proof['root_reaped'])
            self.assertTrue(proof['known_absent'])
            self.assertIsNone(foreign.poll(),'a reused PID must not authorize a signal')
        finally:
            for child in (root,foreign):
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=5)

    def test_output_burst_is_truncated_with_failed_receipt_and_bounded_files(self):
        with tempfile.TemporaryDirectory() as root:
            result,folder=self.invoke(root,
                'import os; data=b"x"*65536; [(os.write(1,data),os.write(2,data)) for _ in range(8)]',
                expect_failure=True,patches=(patch.object(campaign,'MAX_STDIO',4096),))
            self.assertTrue(any(p['truncated'] for p in result['captures'].values()))
            for name in ('stdout.log','stderr.log'):
                self.assertLessEqual((folder/name).stat().st_size,4096)
                self.assertEqual(result[name]['bytes'],result['captures'][name]['retained_bytes'])
                self.assertTrue(result['captures'][name]['reader_joined'])
                self.assertTrue(result['captures'][name]['eof'])
                self.assertEqual(result[name]['sha256'],hashlib.sha256((folder/name).read_bytes()).hexdigest())

    def test_capture_storage_failure_preserves_failure_receipt_and_reaps(self):
        ordinary_open=Path.open
        def faulty_open(path,*args,**kwargs):
            if path.name=='stdout.log' and args and args[0]=='xb':
                raise OSError(errno.ENOSPC,'injected evidence storage fault')
            return ordinary_open(path,*args,**kwargs)
        with tempfile.TemporaryDirectory() as root:
            result,_=self.invoke(root,'import time; print("observed",flush=True); time.sleep(30)',
                expect_failure=True,patches=(patch.object(Path,'open',faulty_open),))
            self.assertTrue(any('injected evidence storage fault' in error
                                for error in result['captures']['stdout.log']['errors']))

    def test_artifact_hash_failure_cannot_suppress_failed_receipt(self):
        with tempfile.TemporaryDirectory() as root:
            result,_=self.invoke(root,'print("small")',expect_failure=True,patches=(
                patch.object(campaign,'digest',side_effect=OSError('injected hash read failure')),))
            self.assertIn('artifact verification failed',result['error'])
            self.assertIn('injected hash read failure',result['stdout.log']['error'])

    def test_normal_child_retains_exact_streams_and_proves_eof(self):
        with tempfile.TemporaryDirectory() as root:
            result,folder=self.invoke(root,'import os; os.write(1,b"a"*256); os.write(2,b"diagnostic")',
                                      expect_failure=False)
            for name,expected in (('stdout.log',b'a'*256),('stderr.log',b'diagnostic')):
                self.assertEqual((folder/name).read_bytes(),expected)
                proof=result['captures'][name]
                self.assertEqual(proof['observed_bytes'],len(expected))
                self.assertEqual(proof['retained_bytes'],len(expected))
                self.assertFalse(proof['truncated'])
                self.assertTrue(proof['eof'] and proof['reader_joined'])
                self.assertEqual(proof['errors'],[])

if __name__=='__main__':
    unittest.main()
