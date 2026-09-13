import json
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch

import package_desktop as p


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()

    def file(self, name, data=b'fixture'):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def manifest(self, libraries=()):
        text = self.file('notices/LICENSE', b'Actual fixture notice bytes\n')
        components = []
        for component in ['adobe-dng-sdk', 'xmp-toolkit', 'rust-dependencies', 'frontend-dependencies']:
            components.append({'component': component, 'libraries': list(libraries),
                               'files': [{'path': 'LICENSE', 'sha256': p.sha256(text)}]})
        path = self.file('notices/manifest.json', json.dumps({'protocol': 1, 'components': components}).encode())
        return path

    def policy(self, platform, system=None):
        value = {'protocol': 1, 'platform': platform, 'library_directories': ['bin'],
                 'system_dependencies': system or {}}
        return self.file('policy.json', json.dumps(value).encode())

    def test_macos_floor_uses_deployment_not_sdk_or_linker_version(self):
        output = ('cmd LC_BUILD_VERSION\n platform MACOS\n minos 12.0\n sdk 26.5\n'
                  ' version 1267.0\ncmd LC_BUILD_VERSION\n platform MACOS\n minos 26.0\n')
        self.assertEqual(p.mac_minimum(output), '26.0')
        self.assertEqual(p.mac_minimum('cmd LC_VERSION_MIN_MACOSX\n cmdsize 16\n version 10.13\n sdk 26.5'), '10.13')
        with self.assertRaises(p.PackageError):
            p.mac_minimum('cmd LC_BUILD_VERSION\n platform IOS\n minos 26.0\n')
        with self.assertRaises(p.PackageError):
            p.mac_minimum('sdk 26.5\n version 1267.0')

    def test_macos_audit_rejects_understated_plist_floor(self):
        exe = self.file('App.app/Contents/MacOS/app')
        self.file('App.app/Contents/Info.plist', plistlib.dumps({'LSMinimumSystemVersion': '12.0'}))
        def tool(*args):
            return 'arm64' if args[0] == 'lipo' else 'cmd LC_BUILD_VERSION\n platform MACOS\n minos 26.0\n'
        with patch.object(p, 'mac_info', return_value=([], [])), patch.object(p, 'run', side_effect=tool):
            with self.assertRaisesRegex(p.PackageError, 'below native closure'):
                p.mac_audit(exe.parent.parent.parent, exe)

    def test_otool_id_is_not_an_edge_and_spaces_survive(self):
        def tool(*args):
            if args[1] == '-D':
                return '/tmp/test.dylib:\n@rpath/test.dylib\n'
            if args[1] == '-L':
                return ('/tmp/test.dylib:\n\t@rpath/test.dylib (compatibility version 1.0.0, current version 1.0.0)\n'
                        '\t/a path/libx.dylib (compatibility version 1.0.0, current version 1.0.0)\n')
            return 'cmd LC_RPATH\n cmdsize 32\n path @loader_path/../lib (offset 12)\n'
        with patch.object(p, 'run', side_effect=tool):
            self.assertEqual(p.mac_info('test'), (['/a path/libx.dylib'], ['@loader_path/../lib']))

    def test_mac_transitive_inherited_rpath_and_cycle(self):
        exe = self.file('bin/app')
        a = self.file('lib/a.dylib')
        b = self.file('lib/b.dylib')
        info = {exe: (['@rpath/a.dylib', '/usr/lib/libSystem.B.dylib'], ['@executable_path/../lib']),
                a: (['@rpath/b.dylib'], []), b: (['@loader_path/a.dylib'], [])}
        with patch.object(p, 'mac_info', side_effect=lambda path: info[path]):
            closure = p.mac_closure(exe)
        self.assertEqual(set(closure), {exe, a, b})
        self.assertEqual(closure[a]['edges']['@rpath/b.dylib'], b)

    def test_mac_missing_ambiguous_and_basename_collision_rejected(self):
        exe = self.file('bin/app')
        a = self.file('lib/a.dylib')
        b = self.file('other/a.dylib', b'different')
        with self.assertRaisesRegex(p.PackageError, 'unresolved or ambiguous'):
            p.resolve_mac('@rpath/missing', exe, exe, [])
        with self.assertRaisesRegex(p.PackageError, 'unresolved or ambiguous'):
            p.resolve_mac('@rpath/a.dylib', exe, exe, [(exe, str(a.parent)), (exe, str(b.parent))])
        with patch.object(p, 'mac_info', return_value=([str(a), str(b)], [])):
            with self.assertRaisesRegex(p.PackageError, 'basename collision'):
                p.mac_closure(exe)

    def test_mac_audit_rejects_external_absolute_dependency(self):
        app = self.root / 'App.app'
        exe = self.file('App.app/Contents/MacOS/app')
        outside = self.file('homebrew/libx.dylib')
        info = {exe: ([str(outside)], []), outside: ([], [])}
        with patch.object(p, 'mac_info', side_effect=lambda path: info[path]), patch.object(p, 'run', return_value='arm64'):
            with self.assertRaisesRegex(p.PackageError, 'external dependency remains'):
                p.mac_audit(app, exe)

    def test_mac_architecture_and_external_rpath_rejected(self):
        app = self.root / 'App.app'
        exe = self.file('App.app/Contents/MacOS/app')
        lib = self.file('App.app/Contents/Frameworks/a.dylib')
        with patch.object(p, 'mac_info', side_effect=lambda path: ([str(lib)], []) if path == exe else ([], [])), \
                patch.object(p, 'run', side_effect=lambda *args: 'arm64' if args[-1] == exe else 'x86_64'):
            with self.assertRaisesRegex(p.PackageError, 'architecture mismatch'):
                p.mac_audit(app, exe)
        with patch.object(p, 'mac_info', return_value=([], ['/opt/homebrew/lib'])), patch.object(p, 'run', return_value='arm64'):
            with self.assertRaisesRegex(p.PackageError, 'external rpath'):
                p.mac_audit(app, exe)

    def test_notices_require_all_components_exact_hash_and_closure(self):
        manifest = self.manifest(['a.dylib'])
        out = self.root / 'copied'
        out.mkdir()
        records = p.notices(manifest, ['a.dylib'], out)
        self.assertEqual(len(records), 4)
        self.assertEqual((out / records[0]['file']).read_bytes(), b'Actual fixture notice bytes\n')
        with self.assertRaisesRegex(p.PackageError, 'missing library notices'):
            p.notices(manifest, ['new.dylib'])
        self.file('notices/LICENSE', b'changed')
        with self.assertRaisesRegex(p.PackageError, 'notice hash mismatch'):
            p.notices(manifest, [])

    def test_missing_static_notices_rejected(self):
        path = self.manifest()
        data = json.loads(path.read_text())
        data['components'].pop()
        path.write_text(json.dumps(data))
        with self.assertRaisesRegex(p.PackageError, 'missing static'):
            p.notices(path, [])

    def test_macos_failure_preserves_input_and_never_publishes_pass(self):
        exe = self.file('App.app/Contents/MacOS/app')
        info = self.file('App.app/Contents/Info.plist', plistlib.dumps({'CFBundleExecutable': 'app', 'LSMinimumSystemVersion': '12.0'}))
        input_hash = p.sha256(exe)
        output = self.root / 'packaged'
        with patch.object(p, 'mac_info', return_value=([], [])), \
                patch.object(p, 'run', side_effect=p.PackageError('signing failed')):
            with self.assertRaisesRegex(p.PackageError, 'signing failed'):
                p.package_macos(info.parent.parent, output, self.manifest(), True)
        self.assertEqual(p.sha256(exe), input_hash)
        self.assertTrue((output / 'App.app').is_dir())
        self.assertFalse((output / 'closure.json').exists())
        self.assertFalse((output / 'App.dmg').exists())
        with self.assertRaisesRegex(p.PackageError, 'new directory'):
            p.package_macos(info.parent.parent, output, self.manifest())

    def test_macos_finalizes_new_copy_before_dmg_and_signs_inside_out(self):
        exe = self.file('App.app/Contents/MacOS/app')
        info = self.file('App.app/Contents/Info.plist', plistlib.dumps({'CFBundleExecutable': 'app', 'LSMinimumSystemVersion': '12.0'}))
        lib = self.file('brew/liba.dylib')
        output = self.root / 'final'
        target_exe = output / 'App.app/Contents/MacOS/app'
        target_lib = output / 'App.app/Contents/Frameworks/liba.dylib'
        states = {exe: ([str(lib)], ['/opt/homebrew/lib']), lib: ([], []),
                  target_exe: ([str(lib)], ['/opt/homebrew/lib']), target_lib: ([], [])}
        commands = []
        def tool(*args):
            commands.append(args)
            if args[0] == 'install_name_tool':
                target = args[-1]
                deps, paths = states[target]
                if args[1] == '-change':
                    deps = [args[3] if value == args[2] else value for value in deps]
                elif args[1] == '-delete_rpath':
                    paths = [value for value in paths if value != args[2]]
                states[target] = (deps, paths)
            if args[0] == 'lipo':
                return 'arm64'
            if args[0] == 'vtool':
                return 'cmd LC_BUILD_VERSION\n platform MACOS\n minos 26.0\n sdk 26.5\n version 1267.0\n'
            if args[:2] == ('hdiutil', 'create'):
                # Installer source is a directory containing the already sealed app.
                image_root = args[args.index('-srcfolder') + 1]
                self.assertTrue((image_root / 'App.app/Contents/MacOS/app').is_file())
                self.assertEqual((image_root / 'Applications').readlink(), Path('/Applications'))
                Path(args[-1]).write_bytes(b'synthetic image')
            return ''
        before = p.sha256(exe)
        with patch.object(p, 'mac_info', side_effect=lambda path: states[path]), patch.object(p, 'run', side_effect=tool):
            result = p.package_macos(info.parent.parent, output, self.manifest(['liba.dylib']), True)
        self.assertEqual(p.sha256(exe), before)
        self.assertEqual(result['status'], 'PASS_DEPENDENCY_CLOSURE_ONLY')
        self.assertTrue((output / 'dmg.json').is_file())
        self.assertEqual(result['declared_minimum'], '26.0')
        self.assertEqual(plistlib.loads(info.read_bytes())['LSMinimumSystemVersion'], '12.0')
        self.assertEqual(states[target_exe][0], ['@executable_path/../Frameworks/liba.dylib'])
        signatures = [args[-1] for args in commands if args[:2] == ('codesign', '--force')]
        self.assertEqual(set(signatures[:-1]), {target_exe, target_lib})
        self.assertEqual(signatures[-1], output / 'App.app')
        self.assertLess(next(i for i, a in enumerate(commands) if a[:2] == ('codesign', '--verify')),
                        next(i for i, a in enumerate(commands) if a[:2] == ('hdiutil', 'create')))

    def test_elf_parser_preserves_needed_runpath_machine(self):
        dynamic = (' 0x1 (NEEDED) Shared library: [libjxl.so.0.11]\n'
                   ' 0x1d (RUNPATH) Library runpath: [$ORIGIN/../lib:$ORIGIN]\n')
        with patch.object(p, 'run', side_effect=[dynamic, '  Machine: Advanced Micro Devices X86-64\n']):
            self.assertEqual(p.elf_info('app'), (['libjxl.so.0.11'], ['$ORIGIN/../lib', '$ORIGIN'],
                                              'Advanced Micro Devices X86-64'))

    def test_elf_runpath_presence_suppresses_rpath_even_when_empty(self):
        for runpath, expected in [('$ORIGIN/run', ['$ORIGIN/run']), ('', [''])]:
            with self.subTest(runpath=runpath):
                dynamic = (' 0xf (RPATH) Library rpath: [$ORIGIN/old]\n'
                           f' 0x1d (RUNPATH) Library runpath: [{runpath}]\n')
                with patch.object(p, 'run', side_effect=[dynamic, ' Machine: X86-64\n']):
                    self.assertEqual(p.elf_info('app')[1], expected)
        with patch.object(p, 'run', side_effect=[' 0xf (RPATH) Library rpath: [$ORIGIN/old]\n',
                                                ' Machine: X86-64\n']):
            self.assertEqual(p.elf_info('app')[1], ['$ORIGIN/old'])

    def test_linux_dependency_only_in_ignored_rpath_cannot_pass(self):
        self.file('bin/app')
        self.file('old/libjxl.so')
        (self.root / 'run').mkdir()
        manifest = self.manifest(['libjxl.so'])
        policy = self.policy('linux')
        # Exercise the real parser and complete audit, not a fabricated elf_info result.
        for runpath, error in [('$ORIGIN/../run', 'unresolved dependency'), ('', 'external/relative')]:
            with self.subTest(runpath=runpath):
                dynamic = (' 0x1 (NEEDED) Shared library: [libjxl.so]\n'
                           ' 0xf (RPATH) Library rpath: [$ORIGIN/../old]\n'
                           f' 0x1d (RUNPATH) Library runpath: [{runpath}]\n')
                def tool(*args):
                    return dynamic if args[1] == '-dW' else ' Machine: X86-64\n'
                with patch.object(p, 'run', side_effect=tool):
                    with self.assertRaisesRegex(p.PackageError, error):
                        p.audit_package('linux', self.root, 'bin/app', policy, manifest)

    def test_pe_includes_delay_imports(self):
        value = ('ImageFileHeader {\n Machine: IMAGE_FILE_MACHINE_AMD64 (0x8664)\n}\n'
                 'Import {\n Name: KERNEL32.dll\n}\nDelayImport {\n Name: WebView2Loader.dll\n}\n')
        with patch.object(p, 'run', return_value=value) as tool:
            names, _, _ = p.pe_info('app.exe')
        self.assertEqual(names, ['KERNEL32.dll', 'WebView2Loader.dll'])
        self.assertEqual(tool.call_args.args, ('llvm-readobj', '--file-headers', '--coff-imports', 'app.exe'))

    def test_pe_all_import_names_preserved_and_missing_non_dll_fails(self):
        self.file('bin/app.exe')
        policy = self.policy('windows')
        manifest = self.manifest()
        for block, name in [('Import', 'HOST.EXE'), ('DelayImport', 'codec.plugin'),
                            ('Import', 'extensionless'), ('DelayImport', 'trailing.dll ')]:
            with self.subTest(block=block, name=name):
                value = ('ImageFileHeader {\n Machine: IMAGE_FILE_MACHINE_AMD64 (0x8664)\n}\n'
                         f'{block} {{\n Name: {name}\n'
                         ' Import {\n Symbol: nested_symbol (0)\n Address: 0x0\n }\n}\n')
                with patch.object(p, 'run', return_value=value):
                    self.assertEqual(p.pe_info('app.exe')[0], [name])
                    with self.assertRaisesRegex(p.PackageError, 'unresolved dependency'):
                        p.audit_package('windows', self.root, 'bin/app.exe', policy, manifest)

    def test_pe_import_name_cannot_disappear_in_malformed_block(self):
        header = 'ImageFileHeader {\n Machine: IMAGE_FILE_MACHINE_AMD64 (0x8664)\n}\n'
        for tail in ['Import {\n}\n', 'DelayImport {\n Name: \n}\n',
                     'Import {\n Name: a.dll\n Name: b.dll\n}\n',
                     'Import {\n Name: a.dll\n']:
            with self.subTest(tail=tail), patch.object(p, 'run', return_value=header + tail):
                with self.assertRaises(p.PackageError):
                    p.pe_info('app.exe')

    def test_linux_installed_origin_closure_with_explicit_system_contract(self):
        exe = self.file('bin/app')
        lib = self.file('lib/libjxl.so')
        info = {exe: (['libjxl.so'], ['$ORIGIN/../lib'], 'arch'),
                lib: (['libc.so.6'], [], 'arch')}
        policy = self.policy('linux', {'libc.so.6': 'glibc >= 2.35 supplied by supported OS'})
        with patch.object(p, 'elf_info', side_effect=lambda path: info[path]):
            result = p.audit_package('linux', self.root, 'bin/app', policy, self.manifest(['libjxl.so']))
        self.assertEqual(len(result['files']), 2)
        self.assertFalse(result['launch_tested'])
        self.assertEqual(list(result['system_dependencies']), ['libc.so.6'])

    def test_linux_build_host_path_or_missing_rpath_cannot_hide_dependency(self):
        self.file('bin/app')
        self.file('bin/libjxl.so')
        manifest = self.manifest(['libjxl.so'])
        policy = self.policy('linux')
        for paths, error in [(['/checkout/.deps/jxl/lib'], 'external/relative'), ([], 'unresolved dependency'),
                             (['$ORIGIN/../../outside'], 'escapes')]:
            with self.subTest(paths=paths), patch.object(p, 'elf_info', return_value=(['libjxl.so'], paths, 'arch')):
                with self.assertRaisesRegex(p.PackageError, error):
                    p.audit_package('linux', self.root, 'bin/app', policy, manifest)

    def test_windows_case_insensitive_closure_and_non_system_runtime_required(self):
        exe = self.file('bin/app.exe')
        dll = self.file('bin/MSVCP140.dll')
        policy = self.policy('windows', {'kernel32.dll': 'Windows 10 system DLL'})
        manifest = self.manifest(['MSVCP140.dll'])
        info = {exe: (['msvcp140.dll'], [], 'AMD64'), dll: (['kernel32.dll'], [], 'AMD64')}
        with patch.object(p, 'pe_info', side_effect=lambda path: info[path]):
            self.assertEqual(len(p.audit_package('windows', self.root, 'bin/app.exe', policy, manifest)['files']), 2)
            dll.unlink()
            with self.assertRaisesRegex(p.PackageError, 'unresolved dependency'):
                p.audit_package('windows', self.root, 'bin/app.exe', policy, manifest)

    def test_package_symlink_escape_and_arch_mismatch_rejected(self):
        exe = self.file('payload/bin/app')
        external = self.file('external/libx.so')
        (exe.parent / 'libx.so').symlink_to(external)
        policy = self.file('payload/policy.json', json.dumps({'protocol': 1, 'platform': 'linux',
                    'library_directories': ['bin'], 'system_dependencies': {}}).encode())
        with patch.object(p, 'elf_info', return_value=(['libx.so'], ['$ORIGIN'], 'arch')):
            with self.assertRaisesRegex(p.PackageError, 'symlink escapes'):
                p.audit_package('linux', self.root / 'payload', 'bin/app', policy, self.manifest())


if __name__ == '__main__':
    unittest.main()
