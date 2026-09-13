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


if __name__ == '__main__':
    unittest.main()
