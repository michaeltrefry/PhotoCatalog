#!/usr/bin/env python3
"""Build installers from staged dependencies, then inspect their installed payload.

Intended for disposable hosted CI runners. NSIS runs its per-user installer;
DEB is extracted without root. A DMG is mounted read-only and its app copied to
an external temporary install directory. No release signing or publication.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

import desktop_tool
import package_desktop as p
import installed_worker_smoke


def bundle(desktop, kind, config=None):
    extra = ['--bundles', kind]
    if config is not None: extra += ['--config', str(config)]
    subprocess.run(desktop_tool.command('bundle', extra), cwd=desktop,
        env=desktop_tool.local_build_environment(os.environ), check=True, timeout=900)


def one(folder, pattern):
    files = sorted(folder.glob(pattern))
    p.require(len(files) == 1, f'expected one {pattern} installer in {folder}, found {files}')
    return files[0]


def bundle_config(platform, stage, notices):
    native = stage/'native'
    if platform == 'linux':
        files = {f'/usr/lib/photocatalog-desktop/native/{f.name}': str(f) for f in native.iterdir()}
        files.update({f'/usr/lib/photocatalog-desktop/THIRD_PARTY_NOTICES/{f.name}': str(f)
                      for f in notices.iterdir() if f.is_file()})
        return {'bundle': {'linux': {'deb': {'files': files,
            'depends': p.json_file(stage/'stage.json')['depends']}}}}
    resources = {str(f): f.name for f in native.iterdir()}
    resources.update({str(f): 'THIRD_PARTY_NOTICES/'+f.name for f in notices.iterdir() if f.is_file()})
    return {'bundle': {'resources': resources, 'windows': {'nsis': {'installMode': 'currentUser'}}}}


def verify_staged_payload(platform, installed, executable, stage):
    record = p.json_file(stage/'stage.json')
    p.require(record['platform'] == platform, 'staged platform mismatch')
    p.require(p.sha256(executable) == record['executable_sha256'], 'installer changed staged executable')
    native = installed/'usr/lib/photocatalog-desktop/native' if platform == 'linux' else installed
    for name, checksum in record['native'].items():
        p.require(Path(name).name == name, 'staged native name is not local')
        p.require(p.sha256(p.regular(native/name)) == checksum, f'installer changed native payload: {name}')


def qualify(args):
    root, output = args.checkout.resolve(), args.output.absolute()
    output.mkdir(parents=True, exist_ok=False)
    desktop, release = root/'desktop', args.target.resolve()/'release'
    installer = None
    installed = output/'installed'; installed.mkdir()
    # Preserve install tree and failed output for CI artifacts; never modify an
    # existing application. The hosted runner owns final workspace disposal.
    p.require(not installed.is_relative_to(root), 'install proof must be outside checkout')
    if args.platform == 'macos':
        bundle(desktop, 'app')
        app = one(release/'bundle/macos', '*.app')
        p.package_macos(app, output/'package', args.notices/'manifest.json', True)
        installer = one(output/'package', '*.dmg')
        mount = output/'mount'; mount.mkdir()
        p.run('hdiutil', 'attach', '-readonly', '-nobrowse', '-mountpoint', mount, installer)
        try:
            source = one(mount, '*.app')
            app = installed/source.name
            shutil.copytree(source, app, symlinks=True)
        finally:
            p.run('hdiutil', 'detach', mount)
        executable = app/'Contents/MacOS/photocatalog-desktop'
        closure = p.mac_audit(app, executable)
    else:
        config = output/'bundle-config.json'
        p.write_json(config, bundle_config(args.platform, args.stage.resolve(), args.notices.resolve()))
        kind = 'deb' if args.platform == 'linux' else 'nsis'
        bundle(desktop, kind, config)
        if args.platform == 'linux':
            installer = one(release/'bundle/deb', '*.deb')
            p.run('dpkg-deb', '-x', installer, installed)
            executable = installed/'usr/bin/photocatalog-desktop'
            manifest = installed/'usr/lib/photocatalog-desktop/THIRD_PARTY_NOTICES/manifest.json'
        else:
            installer = one(release/'bundle/nsis', '*-setup.exe')
            subprocess.run([str(installer), '/S', f'/D={installed}'], check=True, timeout=300)
            executable = installed/'photocatalog-desktop.exe'
            manifest = installed/'THIRD_PARTY_NOTICES/manifest.json'
        verify_staged_payload(args.platform, installed, executable, args.stage)
        closure = p.audit_package(args.platform, installed, str(executable.relative_to(installed)),
                                  args.stage/'policy.json', manifest)
        shutil.copy2(installer, output/installer.name)
    p.write_json(output/'installed-closure.json', closure)
    probe = args.probe.resolve()
    if args.platform == 'linux':
        # The observer executable is not shipped. Give its own SDK dependencies
        # an explicit local path so it can also launch with LD_* removed.
        private_probe = output/'worker-smoke-probe'
        shutil.copy2(probe, private_probe)
        p.run('patchelf', '--set-rpath', str(installed/'usr/lib/photocatalog-desktop/native'), private_probe)
        probe = private_probe
    elif args.platform == 'windows':
        # The observer must also start without the developer PATH. Its private
        # app-local runtime comes from the audited installed payload.
        observer = output/'observer'; observer.mkdir()
        for library in installed.glob('*.dll'):
            shutil.copy2(library, observer/library.name)
        private_probe = observer/probe.name
        shutil.copy2(probe, private_probe)
        probe = private_probe
    smoke = installed_worker_smoke.run(probe, executable, output/'worker-smoke', args.platform)
    p.write_json(output/'qualification.json', {'status': 'PASS_INSTALL_PAYLOAD_AND_WORKERS_ONLY',
        'platform': args.platform, 'installer': str(installer), 'installer_sha256': p.sha256(installer),
        'installed_root': str(installed), 'closure': 'installed-closure.json', 'workers': smoke,
        'gui_tested': False, 'signing': 'ad-hoc/no release credentials',
        'linux_portability': 'Distribution dependencies recorded; not an AppImage qualification' if args.platform == 'linux' else None})


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('checkout', 'target', 'notices', 'probe', 'output'):
        parser.add_argument('--'+name, type=Path, required=True)
    parser.add_argument('--stage', type=Path)
    parser.add_argument('--platform', choices=['macos', 'linux', 'windows'], required=True)
    qualify(parser.parse_args())
