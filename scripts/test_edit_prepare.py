import hashlib
from pathlib import Path
import tempfile
import time
import unittest
import edit_prepare as preparation


class SourceCopyAdmission(unittest.TestCase):
    def test_copy_is_independent_and_preserves_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();source=root/'original';source.write_bytes(b'unchanged original')
            sha=hashlib.sha256(source.read_bytes()).hexdigest()
            result=preparation.source_copy(source,root/'copy',1024,sha,0,time.monotonic()+30)
            self.assertEqual((root/'copy').read_bytes(),source.read_bytes())
            self.assertEqual(result['sha256'],sha)
            self.assertNotEqual(source.stat().st_ino,(root/'copy').stat().st_ino)

    def test_small_limit_refuses_before_creating_output(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();source=root/'original';source.write_bytes(b'123456789')
            with self.assertRaises(ValueError):preparation.source_copy(source,root/'copy',2,'a'*64,0,time.monotonic()+30)
            self.assertFalse((root/'copy').exists())
            self.assertEqual(source.read_bytes(),b'123456789')

    def test_wrong_expected_bytes_preserve_failed_copy_and_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();source=root/'original';source.write_bytes(b'original')
            with self.assertRaises(ValueError):preparation.source_copy(source,root/'copy',1024,'a'*64,0,time.monotonic()+30)
            self.assertEqual(source.read_bytes(),b'original')
            self.assertEqual((root/'copy').read_bytes(),b'original')


class PreparationHostFailures(unittest.TestCase):
    def run_failure(self,stage):
        import json
        from types import SimpleNamespace
        from unittest.mock import patch
        from contextlib import ExitStack
        import edit_qualification as q
        class Host:
            error=None
            def __enter__(self):
                if stage=='before-copy':self.error='injected initial observer failure'
                return self
            def __exit__(self,*_):pass
        host=Host()
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();original=root/'original.data';original.write_bytes(b'unchanged tiny source')
            sha=hashlib.sha256(original.read_bytes()).hexdigest()
            manifest=dict(version=1,inputs=[dict(id=i,path=str(original),sha256=sha,width=1,height=1) for i in q.IDS])
            manifest_path=root/'manifest.json';manifest_path.write_text(json.dumps(manifest))
            descriptor=dict(path=str(manifest_path),sha256=hashlib.sha256(manifest_path.read_bytes()).hexdigest())
            copies=[];launches=[];source_copy=preparation.source_copy
            def copied(*args):
                result=source_copy(*args);copies.append(result)
                if stage=='after-copy' or (stage=='after-background' and len(copies)==31):
                    host.error='injected post-copy observer failure'
                return result
            def generated(*args):
                launches.append(args)
                host.error='injected post-generator observer failure'
            with ExitStack() as stack:
                stack.enter_context(patch.object(preparation.edit_binding,'admit_imports'))
                stack.enter_context(patch.object(preparation.edit_binding,'validate_runtime'))
                stack.enter_context(patch.object(preparation,'host_identity',return_value={'fixture':True}))
                observer=stack.enter_context(patch.object(preparation,'HostObservation',return_value=host))
                stack.enter_context(patch.object(preparation.psutil,'disk_usage',return_value=SimpleNamespace(free=10**15)))
                stack.enter_context(patch.object(preparation,'source_copy',side_effect=copied))
                stack.enter_context(patch.object(preparation.edit_build_plan,'python_command',return_value=['never-executed-generator']))
                stack.enter_context(patch.object(preparation.edit_campaign,'invoke',side_effect=generated))
                with self.assertRaisesRegex(RuntimeError,'preparation stopped; no retry:.*host evidence failed'):
                    preparation.prepare(descriptor,dict(helper_package={},python_runtime={}),root/'prepared')
            result=json.loads((root/'prepared/preparation.json').read_text())
            self.assertFalse(result['complete']);self.assertNotIn('copy_receipt',result)
            self.assertEqual(original.read_bytes(),b'unchanged tiny source')
            self.assertEqual(observer.call_args.kwargs,dict(max_bytes=512*q.MIB,max_record_bytes=q.MIB))
            self.assertEqual(result['preparation_owner'],preparation.edit_disk_budget.preparation_owner())
            start=json.loads((root/'prepared/start.json').read_text())
            self.assertEqual(start['deadline_seconds'],3600,'inner source preparation deadline changed')
            self.assertEqual(start['preparation_owner']['deadline_seconds'],3900)
            if launches:
                self.assertEqual(launches[0][2]['deadline_seconds'],600,'generator deadline changed')
                self.assertEqual(launches[0][2]['process_rss_bytes'],q.GIB)
            for proof in copies:
                self.assertEqual(Path(proof['path']).read_bytes(),original.read_bytes())
            return len(copies),len(launches),len(result['copies_observed'])

    def test_known_initial_observer_error_prevents_first_copy_or_generator(self):
        self.assertEqual(self.run_failure('before-copy'),(0,0,0))

    def test_error_after_actual_source_copy_retains_copy_and_stops_next_work(self):
        self.assertEqual(self.run_failure('after-copy'),(1,0,1))

    def test_error_after_background_copy_prevents_generator_launch(self):
        self.assertEqual(self.run_failure('after-background'),(31,0,30))

    def test_error_after_first_generator_prevents_remaining_six_launches(self):
        self.assertEqual(self.run_failure('after-generator'),(31,1,30))


class PreparedPlanOwnerAdmission(unittest.TestCase):
    def test_old_or_changed_preparation_caps_cannot_be_relabelled_in_new_plan(self):
        import copy
        import edit_qualification as q
        import edit_fixtures
        import edit_disk_budget as disk
        import edit_build_plan as plans
        manifest=dict(version=1,inputs=[dict(id=i,path='/owned/'+i,sha256='a'*64,width=1,height=1) for i in q.IDS])
        prepared=dict(root='/owned/prepared',sources=[dict(id=i) for i in (*q.IDS,*edit_fixtures.FIXTURES)])
        for owner in (None,dict(disk.preparation_owner(),deadline_seconds=3901)):
            value=copy.deepcopy(prepared)
            if owner is not None:value['preparation_owner']=owner
            with self.assertRaisesRegex(ValueError,'exact bounded owner/host contract'):
                plans.build_plan(value,{},manifest,Path('/owned/campaign'))

if __name__=='__main__':unittest.main()
