"""Tiny immutable-package admission fixtures; no real environment hashing."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import edit_binding as binding


class FrozenPackageContracts(unittest.TestCase):
    def make_package(self,root):
        for name in binding.package_paths():
            path=root/name
            path.parent.mkdir(parents=True,exist_ok=True)
            path.write_text('# fixed tiny helper\n')
        return dict(root=str(root),files=binding.collect_package(root))

    def test_changed_imported_helper_rejected_before_import(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve()
            value=self.make_package(root)
            binding.validate_package(value)
            (root/'scripts/edit_reference.py').write_text('raise RuntimeError("must never import")\n')
            with self.assertRaises(ValueError): binding.admit_imports(value)

    def test_missing_observer_or_extra_unbound_helper_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve()
            value=self.make_package(root)
            del value['files']['benchmarks/observe_host.py']
            with self.assertRaises(ValueError): binding.validate_package(value)

    def test_registry_missing_or_mutated_case_rejected(self):
        cases=[dict(id=f'case-{i}',recipe={'exposure':0}) for i in range(533)]
        sha=hashlib.sha256(json.dumps(cases,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        binding.verify_case_registry(sha,cases)
        with self.assertRaises(ValueError): binding.verify_case_registry(sha,cases[:-1])
        cases[100]['recipe']['exposure']=1
        with self.assertRaises(ValueError): binding.verify_case_registry(sha,cases)

    def test_foreign_already_loaded_helper_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve()
            value=self.make_package(root)
            # edit_binding itself is already imported from this test worktree;
            # it cannot masquerade as the copied private helper module.
            with self.assertRaises(ValueError): binding.admit_imports(value)

    def test_runtime_dependency_bytes_not_only_versions_are_bound(self):
        original=dict(executable='/python',executable_sha256='a',prefix='/env',base_prefix='/base',
                      version='v',cache_tag='tag',platform='x',environment={},distributions={'numpy':{'files':{'extension.so':'old'}}},import_closure={},framework=None)
        changed={**original,'distributions':{'numpy':{'files':{'extension.so':'new'}}}}
        with patch.object(binding,'runtime_identity',return_value=changed):
            with self.assertRaises(ValueError): binding.validate_runtime(original)

    def test_valid_bytecode_changes_are_bound_even_when_source_is_unchanged(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve()
            (root/'module.py').write_text('value=1\n')
            cache=root/'__pycache__';cache.mkdir()
            bytecode=cache/'module.cpython-test.pyc';bytecode.write_bytes(b'first-cache')
            with patch.object(binding.sys,'path',[str(root)]):
                first=binding.import_closure()
                bytecode.write_bytes(b'other-cache')
                self.assertNotEqual(binding.import_closure(),first)

    def test_unrecorded_import_shadow_is_detected(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve()
            (root/'known.py').write_text('value=1\n')
            with patch.object(binding.sys,'path',[str(root)]):
                first=binding.import_closure()
                (root/'unexpected.py').write_text('raise RuntimeError("unbound")\n')
                self.assertNotEqual(binding.import_closure(),first)

    def test_framework_library_and_app_bytes_are_bound_under_actual_base(self):
        with tempfile.TemporaryDirectory() as folder:
            base=Path(folder).resolve()
            library=base/'Python';library.write_bytes(b'framework-v1')
            app=base/'Resources/Python.app/Contents/MacOS/Python'
            app.parent.mkdir(parents=True);app.write_bytes(b'app-v1')
            with patch.object(binding.sys,'platform','darwin'), \
                 patch.object(binding.sys,'base_prefix',str(base)), \
                 patch.object(binding.sysconfig,'get_config_var',return_value='Python'):
                first=binding.framework_identity()
                self.assertEqual(first['library']['path'],str(library))
                self.assertEqual(first['app_executable']['path'],str(app))
                original=dict(executable='/launcher',executable_sha256='same',prefix='/env',base_prefix=str(base),
                    version='v',cache_tag='tag',platform='darwin',environment={},distributions={},import_closure={},framework=first)
                for artifact in (library,app):
                    previous=artifact.read_bytes();artifact.write_bytes(previous+b'-modified')
                    changed={**original,'framework':binding.framework_identity()}
                    with patch.object(binding,'runtime_identity',return_value=changed):
                        with self.assertRaisesRegex(ValueError,'framework'):
                            binding.validate_runtime(original)
                    artifact.write_bytes(previous)

    def test_framework_missing_app_refuses_before_runtime_admission(self):
        with tempfile.TemporaryDirectory() as folder:
            base=Path(folder).resolve();(base/'Python').write_bytes(b'framework')
            with patch.object(binding.sys,'platform','darwin'), \
                 patch.object(binding.sys,'base_prefix',str(base)), \
                 patch.object(binding.sysconfig,'get_config_var',return_value='Python'):
                with self.assertRaises(FileNotFoundError):binding.framework_identity()

    def test_nonframework_runtime_does_not_inspect_framework_paths(self):
        with patch.object(binding,'file_hash',side_effect=AssertionError('unexpected framework read')):
            with patch.object(binding.sys,'platform','linux'):
                self.assertIsNone(binding.framework_identity())
            with patch.object(binding.sys,'platform','darwin'), \
                 patch.object(binding.sysconfig,'get_config_var',return_value=''):
                self.assertIsNone(binding.framework_identity())


if __name__=='__main__':
    unittest.main()
