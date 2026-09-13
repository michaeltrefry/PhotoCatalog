import unittest
from unittest import mock
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


if __name__ == '__main__':
    unittest.main()
