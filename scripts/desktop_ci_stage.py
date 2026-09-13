#!/usr/bin/env python3
"""Stage exact platform-native dependencies and notice inputs before Tauri bundles.

Only built artifacts under the supplied output tree are changed. Linux OS
libraries remain package-manager dependencies; custom SDK JPEG XL is local.
Windows runtime libraries are copied only from vcpkg or VS redist, never from a
compiler directory. Unknown imports fail before installer qualification.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

import collect_desktop_notices as notices
import package_desktop as p

WINDOWS_SYSTEM = set(('kernel32 ntdll user32 gdi32 advapi32 ole32 oleaut32 shell32 shlwapi '
    'comdlg32 comctl32 crypt32 secur32 ws2_32 iphlpapi userenv version winmm winspool '
    'setupapi cfgmgr32 bcrypt bcryptprimitives powrprof psapi imm32 dwmapi uxtheme propsys '
    'shcore d3d11 d3d12 dxgi d2d1 dwrite windowscodecs winhttp urlmon wininet msimg32 '
    'rpcrt4 normaliz dbghelp dbgcore dhcpcsvc ncrypt ntmarta wldap32 netapi32 wtsapi32 '
    'msvcrt ucrtbase hid wintrust imagehlp win32u mswsock dnsapi avrt').split())


def system_windows(name):
    lower = name.lower()
    return (lower.endswith('.dll') and lower[:-4] in WINDOWS_SYSTEM) or bool(
        re.fullmatch(r'(?:api|ext)-ms-win-[a-z0-9-]+\.dll', lower))


def copy_library(source, destination):
    source = Path(source).resolve(strict=True)
    target = destination / source.name
    if target.exists():
        p.require(p.sha256(target) == p.sha256(source), 'native basename collision')
    else:
        shutil.copy2(source, target)
    return target


def linux_system(needed, cache):
    paths = {str(Path(value).resolve()) for value in cache.get(needed, ())}
    p.require(len(paths) == 1, f'unknown/ambiguous system SONAME {needed}: {paths}')
    path = paths.pop()
    # Merged-/usr hosts can retain the original /lib spelling in dpkg's
    # database. Match only owners whose recorded file resolves to this ELF.
    aliases = {path, *cache.get(needed, ())}
    if path.startswith('/usr/lib/'):
        aliases.add(path.removeprefix('/usr'))
    owner = []
    for alias in sorted(aliases):
        result = subprocess.run(['dpkg-query', '-S', alias], capture_output=True,
                                text=True, timeout=120, check=False)
        p.require(result.returncode in {0, 1}, f'dpkg ownership lookup failed: {alias}')
        if result.returncode == 0:
            owner.extend(result.stdout.strip().splitlines())
    # dpkg reports arch qualifiers before the final colon-space.
    packages = {line.rsplit(': ', 1)[0] for line in owner if ': ' in line
                and str(Path(line.rsplit(': ', 1)[1]).resolve()) == path}
    p.require(len(packages) == 1, f'unknown system package owner: {needed}')
    package = packages.pop()
    version = p.run('dpkg-query', '-W', '-f=${Version}', package).strip()
    p.require(version and re.fullmatch(r'[a-z0-9+.-]+(?::[a-z0-9]+)?', package), 'invalid distro dependency')
    return {'package': package, 'version': version, 'path': path}


def linux_stage(executable, prefix, output):
    native = output/'native'; native.mkdir()
    for source in sorted((prefix/'lib').glob('*.so*')):
        if source.is_file():
            # Preserve SONAME aliases as regular copies; package loaders must not
            # resolve symlinks into a developer checkout.
            target = native/source.name
            target.write_bytes(source.read_bytes()); target.chmod(0o755)
    p.require(any(native.iterdir()), 'SDK JPEG XL shared libraries missing')
    for target in native.iterdir():
        p.run('patchelf', '--set-rpath', '$ORIGIN', target)
    p.run('patchelf', '--set-rpath', '$ORIGIN/../lib/photocatalog-desktop/native', executable)
    cache = {}
    needed_names = {n for f in [executable, *native.iterdir()] for n in p.elf_info(f)[0] if not (native/n).is_file()}
    machine = p.elf_info(executable)[2]
    for name, path in re.findall(r'^\s*(\S+) \([^\n]+\) => (\S+)$', p.run('ldconfig', '-p'), re.M):
        # Prefer the build machine architecture by matching actual ELF machine.
        if name in needed_names and Path(path).is_file() and p.elf_info(Path(path))[2] == machine:
            cache.setdefault(name, set()).add(path)
    system = {}
    for target in [executable, *native.iterdir()]:
        for name in p.elf_info(target)[0]:
            if (native/name).is_file():
                continue
            system[name] = linux_system(name, cache)
    policy = {'protocol': 1, 'platform': 'linux', 'library_directories': ['usr/lib/photocatalog-desktop/native'],
              'system_dependencies': {name: f"Debian {v['package']} >= {v['version']}" for name,v in system.items()}}
    depends = sorted({f"{v['package']} (>= {v['version']})" for v in system.values()} |
                     {'libwebkit2gtk-4.1-0', 'libgtk-3-0'})
    return native, policy, {'system_packages': system}, depends


def windows_stage(executable, installed, visual_studio, output):
    native = output/'native'; native.mkdir()
    redists = list((visual_studio/'VC/Redist/MSVC').glob('*/x64/Microsoft.VC*.CRT'))
    p.require(redists, 'no licensed x64 Visual Studio CRT redistributable directory')
    redist = max(redists, key=lambda x: tuple(int(n) for n in x.parts[-3].split('.') if n.isdigit()))
    search = [installed/'bin', redist]
    available = {}
    for directory in search:
        for source in directory.glob('*.dll'):
            key = source.name.lower()
            if key in available:
                p.require(p.sha256(source) == p.sha256(available[key]), 'ambiguous Windows runtime')
            available[key] = source
    queue, seen, system, origins = [executable], set(), {}, {}
    while queue:
        target = queue.pop()
        if target in seen: continue
        p.require(len(seen) < 512, 'Windows dependency bound'); seen.add(target)
        for name in p.pe_info(target)[0]:
            lower = name.lower()
            if lower in available:
                copied = copy_library(available[lower], native); queue.append(copied)
                origins[copied.name] = 'msvc' if available[lower].parent == redist else 'vcpkg'
            else:
                p.require(system_windows(name), f'unknown Windows import: {name}')
                system[lower] = 'Windows 10/11 OS API; exact import name, no developer DLL search path'
    return native, {'protocol': 1, 'platform': 'windows', 'library_directories': ['.'],
                    'system_dependencies': system}, {'visual_studio': str(visual_studio), 'redist': str(redist), 'library_origins': origins}


def write_component(output, name, files, libraries):
    p.require(files, f'missing native notice material: {name}')
    entries = []
    for index, (filename, data) in enumerate(sorted(files.items())):
        p.require(0 < len(data) <= 16*1024*1024, 'native material size bound')
        target = output/f'{name}-{index}-{Path(filename).name}'
        target.write_bytes(data)
        entries.append({'path': target.name, 'sha256': hashlib.sha256(data).hexdigest()})
    return {'component': name, 'libraries': sorted(libraries), 'files': entries}


def libraw_source(vcpkg):
    directory = vcpkg/'ports/libraw'
    recipe = (directory/'portfile.cmake').read_text()
    meta = json.loads((directory/'vcpkg.json').read_text())
    version = next(meta[k] for k in ('version', 'version-string', 'version-semver') if k in meta)
    files = {str(f.relative_to(directory)): f.read_bytes() for f in directory.rglob('*') if f.is_file()}
    blocks = re.findall(r'vcpkg_from_github\((.*?)\n\)', recipe, re.S)
    p.require(len(blocks) == 2, 'LibRaw source recipe changed; review exact source inputs')
    for index, block in enumerate(blocks):
        repo = re.search(r'\bREPO\s+([^\s]+)', block)[1]
        revision = re.search(r'\bREF\s+([^\s]+)', block)[1].strip('"').replace('${VERSION}', version)
        checksum = re.search(r'\bSHA512\s+([0-9a-f]{128})', block)[1]
        p.require(repo.startswith('LibRaw/') and re.fullmatch(r'[0-9A-Za-z._-]+', revision), 'LibRaw recipe source')
        data = notices.download(f'https://github.com/{repo}/archive/{revision}.tar.gz', 16*1024*1024)
        p.require(hashlib.sha512(data).hexdigest() == checksum, 'LibRaw source recipe checksum mismatch')
        files[f'corresponding-source-{index}.tar.gz'] = data
    return files


def native_notices(platform, args, native, provenance, output):
    output.mkdir()
    libraries = [f.name for f in native.iterdir()]
    components = []
    if platform == 'linux':
        source = args.sdk/'libjxl/libjxl'
        files = notices.local_notices(source)
        p.require('LICENSE' in files, 'JPEG XL source license missing')
        components.append(write_component(output, 'sdk-jpeg-xl', files, libraries))
        provenance['sdk_source_license'] = str(source/'LICENSE')
    else:
        vcpkg_material = {}
        for directory in sorted((args.vcpkg/'installed/x64-windows-static-md/share').iterdir()):
            if not directory.is_dir(): continue
            copyright_file = directory/'copyright'
            p.require(copyright_file.is_file(), f'vcpkg installed port copyright absent: {directory.name}')
            files = {'copyright': copyright_file.read_bytes()}
            sbom = directory/'vcpkg.spdx.json'
            if sbom.is_file(): files['vcpkg.spdx.json'] = sbom.read_bytes()
            if directory.name == 'libraw': files.update(libraw_source(args.vcpkg))
            vcpkg_material.update({directory.name+'/'+name: data for name, data in files.items()})
        components.append(write_component(output, 'vcpkg-native', vcpkg_material,
            [name for name, origin in provenance['library_origins'].items() if origin == 'vcpkg']))
        # Runtime distribution remains byte-for-byte from the licensed VS redist.
        terms = {str(f.relative_to(args.visual_studio)): f.read_bytes()
                 for f in (args.visual_studio/'Licenses').rglob('*') if f.is_file() and
                 f.suffix.lower() in {'.htm', '.html', '.rtf', '.txt'}}
        p.require(terms, 'Visual Studio license material missing; no invented redistribution terms')
        components.append(write_component(output, 'windows-native-runtime', terms,
            [name for name, origin in provenance['library_origins'].items() if origin == 'msvc']))
    value = {'protocol': 1, 'platform': platform, 'components': components, 'provenance': provenance}
    p.write_json(output/'manifest.json', value)
    return output/'manifest.json'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--platform', choices=['linux', 'windows'], required=True)
    parser.add_argument('--executable', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--sdk', type=Path, required=True)
    parser.add_argument('--jxl-prefix', type=Path)
    parser.add_argument('--vcpkg', type=Path)
    parser.add_argument('--visual-studio', type=Path)
    args = parser.parse_args()
    output = args.output.absolute(); output.mkdir(parents=True, exist_ok=False)
    executable = args.executable.resolve(strict=True)
    if args.platform == 'linux':
        native, policy, provenance, depends = linux_stage(executable, args.jxl_prefix.resolve(), output)
    else:
        native, policy, provenance = windows_stage(executable, args.vcpkg/'installed/x64-windows-static-md',
                                                   args.visual_studio, output)
        depends = []
    manifest = native_notices(args.platform, args, native, provenance, output/'native-notices')
    p.write_json(output/'policy.json', policy)
    p.write_json(output/'stage.json', {'platform': args.platform, 'executable_sha256': p.sha256(executable),
        'native': {f.name: p.sha256(f) for f in native.iterdir()}, 'depends': depends,
        'native_notice_input': str(manifest), 'provenance': provenance})


if __name__ == '__main__':
    main()
