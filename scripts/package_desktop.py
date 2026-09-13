#!/usr/bin/env python3
"""Finalize a local macOS app, or audit an extracted Linux/Windows package.

No builds, downloads, system installation, credentials, or publication. See
../docs/DESKTOP_PACKAGING.md for the Tauri ordering and the notice/policy formats.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


class PackageError(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise PackageError(message)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def run(*args):
    # Inspection tools never execute the inspected artifact (in particular: no ldd).
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True,
                            timeout=120, check=False)
    require(result.returncode == 0,
            f"command failed ({result.returncode}): {args!r}\n{result.stderr[-4096:]}")
    return result.stdout


def inside(path, root):
    return Path(path).resolve().is_relative_to(Path(root).resolve())


def regular(path):
    path = Path(path)
    require(path.is_file() and not path.is_symlink(), f'expected regular file: {path}')
    return path


def json_file(path):
    path = regular(path)
    require(path.stat().st_size <= 4 * 1024 * 1024, f'oversized control: {path}')
    return json.loads(path.read_text(encoding='utf-8'))


def write_json(path, value):
    with Path(path).open('x', encoding='utf-8') as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write('\n')


def mac_system(name):
    return name.startswith(('/System/Library/', '/usr/lib/')) and '..' not in Path(name).parts


def mac_info(path):
    # otool -L includes LC_ID_DYLIB itself; it is not a dependency.
    ids = {line.strip() for line in run('otool', '-D', path).splitlines()[1:]}
    deps = []
    for line in run('otool', '-L', path).splitlines():
        match = re.match(r'\s+(.+?) \(compatibility version ', line)
        if match and match[1] not in ids and match[1] not in deps:
            deps.append(match[1])
    commands = run('otool', '-l', path)
    rpaths = re.findall(r'cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset', commands)
    return deps, rpaths


def expand_mac(value, loader, executable):
    for token, base in [('@loader_path', loader.parent), ('@executable_path', executable.parent)]:
        if value == token or value.startswith(token + '/'):
            return base / value[len(token):].lstrip('/')
    if value.startswith('/'):
        return Path(value)
    raise PackageError(f'unsupported loader path: {value}')


def resolve_mac(name, loader, executable, rpaths):
    if name.startswith('@rpath/'):
        candidates = [expand_mac(p, owner, executable) / name[7:] for owner, p in rpaths]
    else:
        candidates = [expand_mac(name, loader, executable)]
    found = {p.resolve() for p in candidates if p.is_file()}
    require(len(found) == 1, f'unresolved or ambiguous dependency {name} from {loader}: {found}')
    return found.pop()


def mac_closure(executable):
    """Discover an exact closure; inherited runpaths keep their declaring loader."""
    executable = Path(executable).resolve()
    queue = [(executable, [])]
    records = {}
    names = {}
    while queue:
        path, inherited = queue.pop(0)
        if path in records:
            continue
        require(len(records) < 512, 'native library closure exceeds 512 files')
        deps, local_rpaths = mac_info(path)
        rpaths = [(path, p) for p in local_rpaths] + inherited
        edges = {}
        for name in deps:
            if mac_system(name):
                continue
            target = resolve_mac(name, path, executable, rpaths)
            require('.framework' not in str(target),
                    f'custom framework needs explicit framework packaging: {target}')
            previous = names.setdefault(target.name, target)
            require(previous == target, f'library basename collision: {previous}, {target}')
            edges[name] = target
            queue.append((target, rpaths))
        records[path] = {'edges': edges, 'rpaths': local_rpaths}
    return records


def notices(manifest_path, libraries, destination=None):
    """Copy verbatim pinned license/notice texts, including statically linked code."""
    manifest_path = Path(manifest_path).resolve()
    manifest = json_file(manifest_path)
    require(manifest.get('protocol') == 1, 'notice protocol must be 1')
    entries = manifest['components']
    require(isinstance(entries, list) and 0 < len(entries) <= 2048, 'notice component bounds')
    covered, components, result = set(), set(), []
    for entry in entries:
        component = entry['component']
        require(re.fullmatch(r'[a-zA-Z0-9._-]+', component) is not None, 'invalid notice component')
        require(component not in components, 'duplicate notice component')
        components.add(component)
        covered.update(entry['libraries'])
        require(0 < len(entry['files']) <= 1024, 'component must carry notice texts')
        for index, item in enumerate(entry['files']):
            source = regular(manifest_path.parent / item['path'])
            require(source.stat().st_size <= 16 * 1024 * 1024, 'notice file too large')
            require(sha256(source) == item['sha256'], f'notice hash mismatch: {source}')
            name = f'{component}-{index}-{source.name}'
            if destination is not None:
                shutil.copyfile(source, destination / name)
            result.append({'component': component, 'file': name, 'sha256': item['sha256']})
    require({'adobe-dng-sdk', 'xmp-toolkit', 'rust-dependencies', 'frontend-dependencies'} <= components,
            'missing static SDK/Rust/frontend notices')
    require(set(libraries) <= covered, f'missing library notices: {sorted(set(libraries) - covered)}')
    return result


def mac_audit(app, executable):
    app = Path(app).resolve()
    closure = mac_closure(executable)
    arches = set(run('lipo', '-archs', executable).split())
    records = []
    for path in closure:
        require(inside(path, app), f'external dependency remains: {path}')
    for path, info in closure.items():
        require(inside(path, app), f'external dependency remains: {path}')
        require(set(run('lipo', '-archs', path).split()) >= arches, f'architecture mismatch: {path}')
        for rpath in info['rpaths']:
            require(rpath.startswith(('@loader_path', '@executable_path')), f'external rpath: {rpath}')
            require(inside(expand_mac(rpath, path, executable), app), f'escaping rpath: {rpath}')
        records.append({'path': str(path.relative_to(app)), 'sha256': sha256(path),
                        'dependencies': [str(p.relative_to(app)) for p in info['edges'].values()],
                        'build_version': run('vtool', '-show-build', path)})
    run('codesign', '--verify', '--deep', '--strict', '--verbose=2', app)
    return {'platform': 'macos', 'architectures': sorted(arches), 'files': records}


def package_macos(source_app, output, manifest, dmg=False):
    source_app, output = Path(source_app).resolve(), Path(output).absolute()
    require(source_app.is_dir() and source_app.suffix == '.app', 'input must be a Tauri .app')
    require(not output.exists(), 'output must be a new directory; failed outputs are preserved')
    require(not inside(output, source_app), 'output cannot be inside input')
    import plistlib
    with regular(source_app / 'Contents/Info.plist').open('rb') as stream:
        executable_name = plistlib.load(stream)['CFBundleExecutable']
    require(Path(executable_name).name == executable_name, 'invalid CFBundleExecutable')
    original = regular(source_app / 'Contents/MacOS' / executable_name)
    # Resolve before copying so original @loader_path dependencies remain meaningful.
    closure = mac_closure(original)
    libraries = [p.name for p in closure if p != original.resolve()]
    notices(manifest, libraries)
    output.mkdir(parents=True)
    app = output / source_app.name
    shutil.copytree(source_app, app, symlinks=True)
    # Existing custom code/resources must not escape the app after relocation.
    for path in app.rglob('*'):
        require(not path.is_symlink() or inside(path, app), f'escaping bundle symlink: {path}')
    executable = app / 'Contents/MacOS' / executable_name
    frameworks = app / 'Contents/Frameworks'
    frameworks.mkdir(exist_ok=True)
    relocated = {original.resolve(): executable}
    for path in closure:
        if path == original.resolve():
            continue
        target = frameworks / path.name
        require(not target.exists(), f'framework destination already exists: {target}')
        shutil.copy2(path, target)
        relocated[path] = target
    for old, target in relocated.items():
        target.chmod(target.stat().st_mode | 0o200)
        if old != original.resolve():
            run('install_name_tool', '-id', '@rpath/' + target.name, target)
        for name, dependency in closure[old]['edges'].items():
            replacement = ('@executable_path/../Frameworks/' if target == executable else '@loader_path/')
            run('install_name_tool', '-change', name, replacement + dependency.name, target)
        for rpath in dict.fromkeys(closure[old]['rpaths']):
            run('install_name_tool', '-delete_rpath', rpath, target)
    notice_dir = app / 'Contents/Resources/THIRD_PARTY_NOTICES'
    notice_dir.mkdir(parents=True, exist_ok=False)
    notice_records = notices(manifest, libraries, notice_dir)
    write_json(notice_dir / 'manifest.json', notice_records)
    # Sign individual dylibs and executable before sealing the app. No credentials.
    for target in relocated.values():
        run('codesign', '--force', '--sign', '-', '--timestamp=none', target)
    run('codesign', '--force', '--sign', '-', '--timestamp=none', app)
    report = mac_audit(app, executable)
    report.update(protocol=1, status='PASS_DEPENDENCY_CLOSURE_ONLY', notices=notice_records,
                  source_executable_sha256=sha256(original), launch_tested=False,
                  distribution_signing='ad-hoc; not notarized')
    write_json(output / 'closure.json', report)
    if dmg:
        # Feed only the completed application to hdiutil, never modify a signed installer.
        image_root = output / 'image-root'
        image_root.mkdir()
        shutil.copytree(app, image_root / app.name, symlinks=True)
        (image_root / 'Applications').symlink_to('/Applications')
        image = output / (source_app.stem + '.dmg')
        run('hdiutil', 'create', '-volname', source_app.stem, '-srcfolder', image_root,
            '-format', 'UDZO', image)
        run('hdiutil', 'verify', image)
        write_json(output / 'dmg.json', {'sha256': sha256(image), 'launch_tested': False})
    return report


def elf_info(path):
    output = run('readelf', '-dW', path)
    needed = re.findall(r'\(NEEDED\).*?\[(.*?)\]', output)
    # DT_RUNPATH suppresses DT_RPATH even when its string is empty.
    runpaths = re.findall(r'\(RUNPATH\).*?\[(.*?)\]', output)
    paths = runpaths if runpaths else re.findall(r'\(RPATH\).*?\[(.*?)\]', output)
    header = run('readelf', '-hW', path)
    machine = re.search(r'^\s*Machine:\s*(.+)$', header, re.M)
    require(machine is not None, f'ELF machine missing: {path}')
    return needed, [p for value in paths for p in value.split(':')], machine[1]


def pe_info(path):
    # LLVM's COFFImports dump includes both Import and DelayImport blocks.
    output = run('llvm-readobj', '--file-headers', '--coff-imports', path)
    needed, depth, importing, name = [], 0, False, None
    for line in output.splitlines():
        opening = re.fullmatch(r'\s*(\w+) \{', line)
        if opening:
            if depth == 0:
                importing = opening[1] in {'Import', 'DelayImport'}
                name = None
            depth += 1
        elif re.fullmatch(r'\s*}', line):
            require(depth > 0, 'unbalanced PE inspection block')
            depth -= 1
            if depth == 0 and importing:
                require(name is not None and name != '', 'PE import block lacks a name')
                needed.append(name)
                importing = False
        elif importing and depth == 1:
            match = re.fullmatch(r'\s*Name: (.*)', line)
            if match:
                require(name is None, 'duplicate PE import name')
                name = match[1]  # Preserve spelling and whitespace; no suffix filter or strip.
    require(depth == 0, 'unterminated PE inspection block')
    machine = re.search(r'^\s*Machine: (.+)$', output, re.M)
    require(machine is not None, f'PE machine missing: {path}')
    return list(dict.fromkeys(needed)), [], machine[1]


def audit_package(platform, root, executable, policy_path, manifest):
    """Audit unpacked install payload. Runtime exceptions are explicit dependencies.

    The policy supplies installed search directories and exact system SONAMEs/DLLs;
    neither PATH, LD_LIBRARY_PATH nor the build host's library cache is consulted.
    """
    root = Path(root).resolve()
    executable = regular(root / executable).resolve()
    require(inside(executable, root), 'executable escapes package')
    policy = json_file(policy_path)
    require(policy.get('protocol') == 1 and policy['platform'] == platform, 'invalid policy')
    system = policy['system_dependencies']
    require(all(isinstance(k, str) and isinstance(v, str) and v.strip() for k, v in system.items()),
            'system dependency requires an installation contract')
    search = [(root / p).resolve() for p in policy['library_directories']]
    require(all(inside(p, root) and p.is_dir() for p in search), 'invalid library directory')
    if platform == 'windows':
        require(all(p == executable.parent for p in search), 'Windows DLLs must be beside executable')
        system = {k.lower(): v for k, v in system.items()}
    info = elf_info if platform == 'linux' else pe_info
    machine = info(executable)[2]
    queue, seen, records, used_system = [executable], set(), [], {}
    while queue:
        path = queue.pop(0)
        if path in seen:
            continue
        require(len(seen) < 512, 'native closure exceeds 512 files')
        seen.add(path)
        needed, rpaths, arch = info(path)
        require(arch == machine, f'architecture mismatch: {path}')
        directories = []
        for rpath in rpaths:
            require(rpath == '$ORIGIN' or rpath.startswith('$ORIGIN/') or rpath == '${ORIGIN}'
                    or rpath.startswith('${ORIGIN}/'), f'external/relative ELF loader path: {rpath}')
            resolved = Path(rpath.replace('${ORIGIN}', str(path.parent)).replace('$ORIGIN', str(path.parent)))
            require(inside(resolved, root), 'ELF loader path escapes package')
            directories.append(resolved.resolve())
        if platform == 'windows':
            directories = search
        edges = []
        for name in needed:
            lookup = name.lower() if platform == 'windows' else name
            require(Path(name).name == name and '/' not in name and '\\' not in name, 'nonlocal dependency name')
            matches = set()
            for directory in directories:
                if not directory.is_dir():
                    continue
                for candidate in directory.iterdir():
                    equal = candidate.name == name if platform == 'linux' else candidate.name.lower() == lookup
                    if equal and candidate.is_file():
                        require(inside(candidate, root), 'library symlink escapes package')
                        matches.add(candidate.resolve())
            require(len(matches) <= 1, f'ambiguous dependency: {name}')
            if matches:
                target = matches.pop()
                edges.append(str(target.relative_to(root)))
                queue.append(target)
            else:
                require(lookup in system, f'unresolved dependency (no installed loader path): {name} from {path}')
                used_system[name] = system[lookup]
        records.append({'path': str(path.relative_to(root)), 'sha256': sha256(path), 'dependencies': edges})
    libraries = [p.name for p in seen if p != executable]
    notice_records = notices(manifest, libraries)
    # Notices must actually be in the payload being audited, not just on the build host.
    require(inside(manifest, root), 'notice manifest must ship in package')
    for component in json_file(manifest)['components']:
        for item in component['files']:
            require(inside(Path(manifest).parent / item['path'], root), 'notice text must ship in package')
    return {'protocol': 1, 'status': 'PASS_DEPENDENCY_CLOSURE_ONLY', 'platform': platform,
            'machine': machine, 'files': records, 'system_dependencies': used_system,
            'notices': notice_records, 'launch_tested': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    mac = commands.add_parser('macos')
    mac.add_argument('--app', required=True, type=Path)
    mac.add_argument('--output', required=True, type=Path)
    mac.add_argument('--notices', required=True, type=Path)
    mac.add_argument('--dmg', action='store_true')
    audit = commands.add_parser('audit')
    audit.add_argument('--platform', choices=['linux', 'windows'], required=True)
    audit.add_argument('--root', required=True, type=Path)
    audit.add_argument('--executable', required=True)
    audit.add_argument('--policy', required=True, type=Path)
    audit.add_argument('--notices', required=True, type=Path)
    audit.add_argument('--report', required=True, type=Path)
    args = parser.parse_args()
    if args.command == 'macos':
        require(sys.platform == 'darwin', 'macOS packaging requires macOS host tools')
        package_macos(args.app, args.output, args.notices, args.dmg)
    else:
        result = audit_package(args.platform, args.root, args.executable, args.policy, args.notices)
        write_json(args.report, result)


if __name__ == '__main__':
    try:
        main()
    except (PackageError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(f'PACKAGING_FAILED: {error}', file=sys.stderr)
        sys.exit(1)
