import unittest
import desktop_tool as d


class DesktopToolTests(unittest.TestCase):
    def test_build_has_no_ambient_signing_or_updater_authority(self):
        original = {'APPLE_ID': 'secret', 'APPLE_API_KEY_PATH': 'secret',
                    'APPLE_KEYCHAIN_PROFILE': 'secret', 'APPLE_SIGNING_IDENTITY': 'release',
                    'APPLE_CERTIFICATE': 'secret', 'TAURI_SIGNING_PRIVATE_KEY': 'secret',
                    'TAURI_SIGNING_PRIVATE_KEY_PASSWORD': 'secret', 'PATH': 'tools', 'CARGO_TARGET_DIR': 'target'}
        result = d.local_build_environment(original)
        self.assertEqual(result, {'PATH': 'tools', 'CARGO_TARGET_DIR': 'target', 'APPLE_SIGNING_IDENTITY': '-'})
        self.assertEqual(original['APPLE_SIGNING_IDENTITY'], 'release')

    def test_locked_build_separates_bundle(self):
        args = d.command('build', [])
        self.assertEqual(args[1:], ['exec', '--', 'tauri', 'build', '--no-bundle', '--', '--locked'])
        self.assertEqual(d.command('bundle', ['--bundles', 'app'])[1:],
                         ['exec', '--', 'tauri', 'bundle', '--bundles', 'app'])
        self.assertEqual(d.command('build', ['--debug'])[1:],
                         ['exec', '--', 'tauri', 'build', '--no-bundle', '--debug', '--', '--locked'])
        with self.assertRaises(ValueError):
            d.command('frontend-test', ['arbitrary'])


if __name__ == '__main__':
    unittest.main()
