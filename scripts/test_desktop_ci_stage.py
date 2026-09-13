import json
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import patch

import desktop_ci_stage as stage
import desktop_ci_package as package
import collect_desktop_notices as notices
import package_desktop as p


class StageTests(unittest.TestCase):
    def test_windows_requires_explicit_os_contract_not_developer_dll(self):
        self.assertTrue(stage.system_windows('KERNEL32.dll'))
        self.assertTrue(stage.system_windows('api-ms-win-core-memory-l1-1-0.dll'))
        for name in ('vcruntime140.dll', 'libraw.dll', 'plugin.exe', 'developer.dll', '../kernel32.dll'):
            self.assertFalse(stage.system_windows(name), name)

    def test_windows_regular_and_delay_names_preserved_and_missing_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); exe = root/'app.exe'; exe.write_bytes(b'app')
            installed = root/'vcpkg'; (installed/'bin').mkdir(parents=True)
            vs = root/'VS'; redist = vs/'VC/Redist/MSVC/14.44.1/x64/Microsoft.VC143.CRT'
            redist.mkdir(parents=True); (redist/'vcruntime140.dll').write_bytes(b'crt')
            output = root/'stage'; output.mkdir()
            def info(path):
                return (['vcruntime140.dll', 'kernel32.dll'] if path == exe else ['kernel32.dll'], [], 'AMD64')
            with patch.object(p, 'pe_info', side_effect=info):
                native, policy, proof = stage.windows_stage(exe, installed, vs, output)
            self.assertEqual((native/'vcruntime140.dll').read_bytes(), b'crt')
            self.assertEqual(proof['library_origins'], {'vcruntime140.dll': 'msvc'})
            self.assertEqual(set(policy['system_dependencies']), {'kernel32.dll'})
            output2 = root/'bad'; output2.mkdir()
            with patch.object(p, 'pe_info', return_value=(['custom.exe'], [], 'AMD64')):
                with self.assertRaisesRegex(p.PackageError, 'unknown Windows import: custom.exe'):
                    stage.windows_stage(exe, installed, vs, output2)

    def test_native_copy_rejects_basename_collision(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); out = root/'out'; out.mkdir()
            lib = root/'x.dll'; lib.write_bytes(b'expected')
            stage.copy_library(lib, out)
            lib.write_bytes(b'changed')
            with self.assertRaisesRegex(p.PackageError, 'collision'):
                stage.copy_library(lib, out)

    def test_linux_requires_exact_installed_package_owner(self):
        owner = subprocess.CompletedProcess([], 0, 'libraw23:amd64: /usr/lib/libraw.so.23\n', '')
        with patch.object(stage.subprocess, 'run', return_value=owner), patch.object(p, 'run', return_value='0.23.1'):
            self.assertEqual(stage.linux_system('libraw.so.23', {'libraw.so.23': {'/usr/lib/libraw.so.23'}})['package'], 'libraw23:amd64')
        with self.assertRaisesRegex(p.PackageError, 'unknown/ambiguous'):
            stage.linux_system('libraw.so.23', {})

    def test_linux_merged_usr_owner_spelling_does_not_hide_dependency(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); actual = root/'usr/lib/libc.so.6'; actual.parent.mkdir(parents=True)
            actual.write_bytes(b'ELF'); (root/'lib').symlink_to(actual.parent)
            alias = root/'lib/libc.so.6'
            owner = subprocess.CompletedProcess([], 0, f'libc6:amd64: {alias}\n', '')
            missing = subprocess.CompletedProcess([], 1, '', '')
            with patch.object(stage.subprocess, 'run', side_effect=[owner, missing]), patch.object(p, 'run', return_value='2.39'):
                self.assertEqual(stage.linux_system('libc.so.6', {'libc.so.6': {str(alias)}})['package'], 'libc6:amd64')

    def test_locked_winapi_parent_fetches_verified_notice_on_cold_cache(self):
        from test_collect_desktop_notices import archive
        data = archive({'LICENSE-MIT': b'exact parent notice'})
        package = {'name': 'winapi', 'version': '0.3.9', 'checksum': notices.digest(data)}
        with tempfile.TemporaryDirectory() as tmp, patch.object(notices, 'download', return_value=data) as fetch:
            _, files, _ = notices.locked_crate(package, Path(tmp), True)
            self.assertEqual(files['LICENSE-MIT'], b'exact parent notice')
            fetch.assert_called_once()
            package['checksum'] = '0'*64
            with self.assertRaisesRegex(ValueError, 'checksum'):
                notices.locked_crate(package, Path(tmp), True)

    def test_bundle_paths_are_actual_loader_and_notice_locations(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); (root/'native').mkdir(); (root/'native/x.so').write_bytes(b'x')
            (root/'stage.json').write_text(json.dumps({'depends': ['libc6 (>= 2.39)']}))
            text = root/'notices'; text.mkdir(); (text/'manifest.json').write_text('{}')
            linux = package.bundle_config('linux', root, text)['bundle']['linux']['deb']
            self.assertIn('/usr/lib/photocatalog-desktop/native/x.so', linux['files'])
            self.assertIn('/usr/lib/photocatalog-desktop/THIRD_PARTY_NOTICES/manifest.json', linux['files'])
            self.assertEqual(linux['depends'], ['libc6 (>= 2.39)'])
            windows = package.bundle_config('windows', root, text)['bundle']
            self.assertEqual(windows['resources'][str(root/'native/x.so')], 'x.so')
            self.assertEqual(windows['windows']['nsis']['installMode'], 'currentUser')

    def test_installer_must_preserve_staged_executable_and_native_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); installed = root/'installed'; installed.mkdir()
            exe = installed/'app.exe'; exe.write_bytes(b'app')
            dll = installed/'native.dll'; dll.write_bytes(b'native')
            (root/'stage.json').write_text(json.dumps({'platform': 'windows',
                'executable_sha256': p.sha256(exe), 'native': {dll.name: p.sha256(dll)}}))
            package.verify_staged_payload('windows', installed, exe, root)
            dll.write_bytes(b'changed')
            with self.assertRaisesRegex(p.PackageError, 'changed native payload'):
                package.verify_staged_payload('windows', installed, exe, root)
            exe.write_bytes(b'rebuilt')
            with self.assertRaisesRegex(p.PackageError, 'changed staged executable'):
                package.verify_staged_payload('windows', installed, exe, root)

    def test_explicit_native_notice_bytes_digest_and_scope(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); out = root/'out'; out.mkdir()
            text = b'Copyright\r\nexact notice\xff'; (root/'copyright').write_bytes(text)
            value = {'protocol': 1, 'platform': 'windows', 'components': [{'component': 'vcpkg-native',
                'libraries': ['x.dll'], 'files': [{'path': 'copyright', 'sha256': notices.digest(text)}]}]}
            control = root/'input.json'; control.write_text(json.dumps(value))
            copied, proof = notices.import_native_input(control, out)
            self.assertEqual((out/copied[0]['files'][0]['path']).read_bytes(), text)
            self.assertEqual(proof['platform'], 'windows')
            (root/'copyright').write_bytes(b'wrong')
            with self.assertRaisesRegex(ValueError, 'digest'):
                notices.import_native_input(control, out)
            value['components'][0]['files'][0]['path'] = '../outside'
            control.write_text(json.dumps(value))
            with self.assertRaisesRegex(ValueError, 'path'):
                notices.import_native_input(control, out)
            value['platform'] = 'macos'; control.write_text(json.dumps(value))
            with self.assertRaisesRegex(ValueError, 'platform'):
                notices.import_native_input(control, out)


if __name__ == '__main__':
    unittest.main()
