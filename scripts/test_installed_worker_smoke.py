import unittest
from unittest import mock
import json
import os
from pathlib import Path
import tempfile
import sys
import edit_campaign
import installed_worker_smoke as smoke


class EnvironmentTests(unittest.TestCase):
    def test_no_developer_loader_or_signing_paths(self):
        value = smoke.environment({'PATH': '/checkout:/opt/homebrew/bin', 'DYLD_LIBRARY_PATH': '/opt',
            'LD_PRELOAD': 'injection', 'APPLE_PASSWORD': 'secret', 'TAURI_SIGNING_KEY': 'secret',
            'HOME': '/home/test', 'PKG_CONFIG_PATH': '/checkout'}, 'macos')
        self.assertEqual(value, {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': '/home/test'})

    def test_windows_path_does_not_inherit_vcpkg(self):
        value = smoke.environment({'SystemRoot': 'C:/Windows', 'PATH': 'C:/vcpkg/bin',
                                   'VCPKG_ROOT': 'C:/vcpkg'}, 'windows')
        self.assertNotIn('vcpkg', value['PATH'])
        self.assertNotIn('VCPKG_ROOT', value)

    def test_windows_uppercase_snapshot_matches_real_os_environ(self):
        value = smoke.environment({'SYSTEMROOT': r'C:\Windows', 'PATH': r'C:\vcpkg\bin'}, 'windows')
        self.assertEqual(value['PATH'], r'C:\Windows\System32;C:\Windows')
        self.assertEqual(value['SYSTEMROOT'], r'C:\Windows')

    def test_windows_filter_and_path_replace_are_case_insensitive(self):
        inherited = {'sYsTeMrOoT': r'C:\Windows', 'Path': r'C:\checkout',
                     'Vcpkg_Root': r'C:\vcpkg', 'Ld_Preload': 'injection',
                     'Dyld_Library_Path': 'injection', 'Apple_Password': 'secret',
                     'Tauri_Signing_Key': 'secret', 'Pkg_Config_Path': 'checkout',
                     'Library_Path': 'checkout', 'Cpath': 'checkout', 'Temp': r'C:\Temp'}
        original = inherited.copy()
        self.assertEqual(smoke.environment(inherited, 'windows'), {
            'SYSTEMROOT': r'C:\Windows', 'PATH': r'C:\Windows\System32;C:\Windows',
            'TEMP': r'C:\Temp'})
        self.assertEqual(inherited, original)

    def test_windows_missing_root_fails_with_actionable_error(self):
        for inherited in ({}, {'SYSTEMROOT': ''}, {'SystemRoot': '  '}):
            with self.subTest(inherited=inherited), self.assertRaisesRegex(
                    smoke.package.PackageError, 'missing SYSTEMROOT'):
                smoke.environment(inherited, 'windows')


class ExecutableAssociationTests(unittest.TestCase):
    def check_observation(self, kind, cleanup=True):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            probe = root/'probe'; probe.write_bytes(b'observer')
            executable = root/'installed'; executable.write_bytes(b'installed identity')
            other = root/'reported'
            if kind == 'hardlink':
                os.link(executable, other)
            elif kind == 'same_bytes_copy':
                other.write_bytes(executable.read_bytes())
            elif kind == 'verbatim':
                other = Path('\\\\?\\' + str(executable))
            elif kind == 'actual':
                other = executable
            reported = str(other)
            if kind == 'relative':
                reported = 'installed'
            elif kind == 'malformed':
                reported = str(root/'invalid') + '\0'
            def invoke(command, output, limits, disk_root):
                output.mkdir()
                (output/'stdout.log').write_text(json.dumps({
                    'status': 'PASS_INSTALLED_PREVIEW_AND_EXPORT_WORKERS',
                    'worker_executable': reported, 'temporary_state_removed': cleanup}))
                return {'ownership': {'root_reaped': True, 'known_absent': True}}
            with mock.patch.object(edit_campaign, 'invoke', side_effect=invoke):
                return smoke.run(probe, executable, root/'evidence', 'macos')

    def test_same_file_with_different_name_is_accepted(self):
        self.assertEqual(self.check_observation('hardlink')['status'], 'PASS_INSTALLED_WORKERS_ONLY')

    def test_equal_bytes_different_file_and_unverifiable_paths_reject(self):
        for kind in ('same_bytes_copy', 'missing', 'relative', 'malformed'):
            with self.subTest(kind=kind), self.assertRaisesRegex(
                    smoke.package.PackageError, 'probe executable association'):
                self.check_observation(kind)

    def test_cleanup_still_requires_literal_true(self):
        for cleanup in (False, 1, 'true', None):
            with self.subTest(cleanup=cleanup), self.assertRaisesRegex(
                    smoke.package.PackageError, 'probe temporary state cleanup'):
                self.check_observation('actual', cleanup)

    @unittest.skipUnless(sys.platform == 'win32', 'native Windows verbatim path')
    def test_windows_verbatim_file_name_is_same_object(self):
        self.assertEqual(self.check_observation('verbatim')['status'], 'PASS_INSTALLED_WORKERS_ONLY')


class WorkingDirectoryLifetimeTests(unittest.TestCase):
    def test_cleanup_restores_cwd_and_environment_before_removal_on_success_and_failure(self):
        temporary_directory=tempfile.TemporaryDirectory
        before_cwd=Path.cwd()
        before_env=dict(os.environ)
        for fail in (False,True):
            with self.subTest(fail=fail), temporary_directory() as folder:
                root=Path(folder).resolve()
                probe=root/'probe';probe.write_bytes(b'synthetic observer identity')
                executable=root/'installed';executable.write_bytes(b'synthetic installed identity')
                observed=[]
                class WindowsDirectory:
                    def __init__(self,**_):
                        pass
                    def __enter__(self):
                        self.temporary=temporary_directory(prefix='test-installed-cwd-')
                        self.path=Path(self.temporary.name).resolve()
                        return str(self.path)
                    def __exit__(self,*_):
                        try:
                            # Model Windows removal denial even on a POSIX host.
                            if Path.cwd()==self.path:
                                raise PermissionError(32,'current directory still in use')
                            self.assert_restored()
                        finally:
                            os.chdir(before_cwd)
                            self.temporary.cleanup()
                    def assert_restored(self):
                        observed.append('cleanup')
                        if Path.cwd()!=before_cwd or dict(os.environ)!=before_env:
                            raise AssertionError('caller state not restored before cleanup')
                failure=RuntimeError('original child failure; retained at synthetic invocation')
                def invoke(command,output,limits,disk_root):
                    self.assertNotEqual(Path.cwd(),before_cwd)
                    self.assertNotIn('DYLD_LIBRARY_PATH',os.environ)
                    observed.append('invoke')
                    output.mkdir()
                    if fail:
                        (output/'result.json').write_text(json.dumps({'error':'synthetic worker failure','root_reaped':True}))
                        raise failure
                    (output/'stdout.log').write_text(json.dumps({
                        'status':'PASS_INSTALLED_PREVIEW_AND_EXPORT_WORKERS',
                        'worker_executable':str(executable),'temporary_state_removed':True}))
                    return {'ownership':{'root_reaped':True,'known_absent':True}}
                with mock.patch.object(smoke.tempfile,'TemporaryDirectory',WindowsDirectory), \
                     mock.patch.object(edit_campaign,'invoke',side_effect=invoke):
                    if fail:
                        with self.assertRaises(RuntimeError) as error:
                            smoke.run(probe,executable,root/'evidence','macos')
                        self.assertIs(error.exception,failure)
                        self.assertTrue((root/'evidence/invoke/result.json').is_file())
                    else:
                        result=smoke.run(probe,executable,root/'evidence','macos')
                        self.assertEqual(result['status'],'PASS_INSTALLED_WORKERS_ONLY')
                self.assertEqual(observed,['invoke','cleanup'])
                self.assertEqual(Path.cwd(),before_cwd)
                self.assertEqual(dict(os.environ),before_env)


if __name__ == '__main__':
    unittest.main()
