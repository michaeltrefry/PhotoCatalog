#!/usr/bin/env python3
"""Collect version-bound third-party notices without invoking package/build tools.

Inputs are a desktop checkout, an installed Homebrew prefix, an existing DNG
SDK and a Cargo cache. Missing official notices are reported, never replaced by
made-up copyright text. Downloads are explicit (--fetch), checksum-pinned for
registry archives and commit-pinned for upstream monorepo notice files.
"""
from __future__ import annotations
import argparse
import base64
import hashlib
import io
import json
from pathlib import Path
import re
import tarfile
import tomllib
import urllib.error
import urllib.request

NATIVE = ['libraw', 'libavif', 'webp', 'jpeg-xl', 'jpeg-turbo', 'libomp',
          'little-cms2', 'dav1d', 'aom', 'highway', 'brotli', 'libvmaf']
NOTICE = re.compile(r'^(?:licen[cs]e|copying|copyright|notice|unlicense|ofl|patents|authors)(?:[._-].*)?$', re.I)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_ref(path):
    data = Path(path).read_bytes()
    return {'path': str(path), 'sha256': digest(data), 'bytes': len(data)}


def download(url, limit):
    request = urllib.request.Request(url, headers={'User-Agent': 'PhotoCatalog-notice-inventory/1'})
    with urllib.request.urlopen(request, timeout=30) as response:
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError('download exceeds bound: ' + url)
    return data


def unpack_crate(data, checksum):
    if digest(data) != checksum:
        raise ValueError('crate checksum mismatch')
    files = {}
    total = 0
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
        for member in archive:
            if not member.isfile():
                continue
            name = member.name.split('/', 1)[-1]
            if name == 'Cargo.toml' or name == '.cargo_vcs_info.json' or NOTICE.fullmatch(Path(name).name):
                if member.size > 16 * 1024 * 1024:
                    raise ValueError('oversized crate notice')
                total += member.size
                if total > 32 * 1024 * 1024:
                    raise ValueError('crate notice total exceeds bound')
                files[name] = archive.extractfile(member).read()
    return files


def verify_npm_runtime(data, integrity, directory):
    algorithm, encoded = integrity.split('-', 1)
    if algorithm not in {'sha512', 'sha256'}:
        raise ValueError('unsupported npm integrity algorithm')
    if hashlib.new(algorithm, data).digest() != base64.b64decode(encoded, validate=True):
        raise ValueError('npm integrity mismatch')
    checked = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
        for member in archive:
            if not member.isfile():
                continue
            name = member.name.split('/', 1)[-1]
            path = Path(name)
            if path.is_absolute() or '..' in path.parts:
                raise ValueError('npm member traversal')
            if NOTICE.fullmatch(path.name) or path.suffix in {'.woff', '.woff2'}:
                if member.size > 16 * 1024 * 1024:
                    raise ValueError('oversized npm notice/font')
                expected = archive.extractfile(member).read()
                actual = (directory/path).read_bytes()
                if actual != expected:
                    raise ValueError('installed runtime notice/font differs from locked archive: ' + name)
                checked[name] = digest(expected)
    return checked


def local_notices(root):
    # Include nested vendored licenses, but never follow directory symlinks.
    return {str(p.relative_to(root)): p.read_bytes() for p in sorted(root.rglob('*'))
            if p.is_file() and not p.is_symlink() and NOTICE.fullmatch(p.name)}


def upstream_notices(meta, vcs, fetch, cache):
    repo = meta.get('repository', meta.get('homepage', '')).removesuffix('.git').rstrip('/')
    repo = repo.split('/tree/')[0]
    if meta['name'] == 'libappindicator-sys':
        repo = 'https://github.com/tauri-apps/libappindicator-rs'
    if meta['name'] == 'rsqlite-vfs':
        repo = 'https://github.com/Spxg/sqlite-wasm-rs'
    match = re.fullmatch(r'https://github.com/([^/]+/[^/]+)', repo)
    revision = vcs.get('git', {}).get('sha1', '')
    if not match or not re.fullmatch(r'[0-9a-f]{40}', revision) or not fetch:
        return {}, []
    key = (match[1], revision)
    if key in cache:
        return cache[key]
    result, sources = {}, []
    for name in ['LICENSE', 'LICENSE.md', 'LICENSE-MIT', 'LICENSE-APACHE', 'LICENSE_MIT', 'LICENSE_APACHE-2.0', 'COPYING']:
        url = f'https://raw.githubusercontent.com/{match[1]}/{revision}/{name}'
        try:
            data = download(url, 1024 * 1024)
        except urllib.error.HTTPError as error:
            if error.code == 404:
                continue
            raise
        result['upstream-' + name] = data
        sources.append({'url': url, 'sha256': digest(data)})
    cache[key] = (result, sources)
    return result, sources


def aggregate(items):
    stream = bytearray()
    for identity, files in items:
        for name, data in sorted(files.items()):
            # Only delimiters are ours; every upstream file remains verbatim.
            stream.extend(f'\n===== {identity} / {name} / SHA256 {digest(data)} =====\n'.encode())
            stream.extend(data)
            stream.extend(b'\n===== END UPSTREAM FILE =====\n')
    return bytes(stream)


def collect(args):
    root, out = args.checkout.resolve(), args.output.absolute()
    out.mkdir(parents=True, exist_ok=False)
    report = {'protocol': 1, 'status': 'NOTICE_INVENTORY_PENDING', 'inputs': {}, 'rust': [],
              'frontend': [], 'native': [], 'missing': [], 'claim':
              'Source notice inventory; exact desktop Mach-O closure and redistribution/source obligations require final package review.'}
    manifest = {'protocol': 1, 'components': []}
    def component(name, items, libraries=()):
        data = aggregate(items)
        if not data:
            report['missing'].append({'component': name, 'reason': 'no notice text'})
            return
        path = out / (name + '.txt')
        path.write_bytes(data)
        manifest['components'].append({'component': name, 'libraries': sorted(libraries),
                                       'files': [{'path': path.name, 'sha256': digest(data)}]})
    for name, path in [('cargo_lock', root/'desktop/src-tauri/Cargo.lock'),
                       ('frontend_lock', root/'desktop/package-lock.json'),
                       ('root_cargo_lock', root/'Cargo.lock'), ('dng_license', args.sdk/'LICENSE.txt')]:
        report['inputs'][name] = file_ref(path)
    cargo = tomllib.loads((root/'desktop/src-tauri/Cargo.lock').read_text())
    rust_items, cache = [], {}
    for index, package in enumerate(cargo['package']):
        identity = f"{package['name']}@{package['version']}"
        entry = {'identity': identity, 'source': package.get('source'), 'checksum': package.get('checksum')}
        if not package.get('source'):
            entry['status'] = 'local project; separately inventoried; no project license invented'
            report['rust'].append(entry)
            continue
        if not package['source'].startswith('registry+'):
            raise ValueError('unsupported non-registry source: ' + identity)
        archive = args.cargo_home/'registry/cache/index.crates.io-1949cf8c6b5b557f'/f"{package['name']}-{package['version']}.crate"
        url = f"https://static.crates.io/crates/{package['name']}/{package['name']}-{package['version']}.crate"
        try:
            if archive.is_file():
                data = archive.read_bytes()
            elif args.fetch:
                data = download(url, 64 * 1024 * 1024)
            else:
                raise FileNotFoundError('no exact cached crate archive')
            files = unpack_crate(data, package['checksum'])
            meta = tomllib.loads(files['Cargo.toml'].decode())['package']
            notices = {name: data for name, data in files.items() if NOTICE.fullmatch(Path(name).name)}
            entry.update(license=meta.get('license'), repository=meta.get('repository'), archive=url)
            if not any(Path(n).parent == Path('.') for n in notices):
                upstream, entry['upstream_notices'] = upstream_notices(meta, json.loads(files.get('.cargo_vcs_info.json', b'{}')), args.fetch, cache)
                notices.update(upstream)
            if meta.get('license') == 'MPL-2.0':
                source_path = out / f"source-{package['name']}-{package['version']}.crate"
                source_path.write_bytes(data)
                entry['included_source'] = file_ref(source_path)
                if not notices and args.fetch:
                    url = 'https://www.mozilla.org/media/MPL/2.0/index.txt'
                    notice = download(url, 1024 * 1024)
                    notices['MPL-2.0.txt'] = notice
                    entry['upstream_notices'] = [{'url': url, 'sha256': digest(notice), 'association': 'exact crate Cargo.toml license MPL-2.0 and included source archive'}]
            if not notices and meta['name'].startswith('winapi-'):
                # Both import-library crates direct users to the locked winapi parent.
                # Carry its actual license texts; retain the explicit family attribution.
                parent = next(p for p in cargo['package'] if p['name'] == 'winapi')
                parent_archive = args.cargo_home/'registry/cache/index.crates.io-1949cf8c6b5b557f'/f"winapi-{parent['version']}.crate"
                parent_files = unpack_crate(parent_archive.read_bytes(), parent['checksum'])
                notices = {n: b for n,b in parent_files.items() if NOTICE.fullmatch(Path(n).name)}
                entry['parent_project_notices'] = {'identity': f"winapi@{parent['version']}", 'checksum': parent['checksum'], 'basis': meta['description']}
            if not notices:
                report['missing'].append({'identity': identity, 'reason': 'upstream notice absent', 'metadata': meta})
            else:
                rust_items.append((identity, notices))
                entry['notices'] = {name: digest(data) for name, data in notices.items()}
        except (OSError, ValueError, KeyError) as error:
            entry['error'] = str(error)
            report['missing'].append({'identity': identity, 'reason': str(error)})
        report['rust'].append(entry)
        if index % 25 == 0:
            print(f'Rust notice records: {index + 1}/{len(cargo["package"])}', flush=True)
    component('rust-dependencies', rust_items)
    sources = sorted(out.glob('source-*.crate'))
    if sources:
        manifest['components'].append({'component': 'mpl-corresponding-source', 'libraries': [],
          'files': [{'path': p.name, 'sha256': digest(p.read_bytes())} for p in sources]})
    lock = json.loads((root/'desktop/package-lock.json').read_text())
    frontend_items = []
    for relative, package in lock['packages'].items():
        if not relative:
            continue
        entry = {'path': relative, 'version': package.get('version'), 'integrity': package.get('integrity'),
                 'resolved': package.get('resolved'), 'license': package.get('license')}
        if package.get('dev'):
            entry['status'] = 'build/test tool; not frontend runtime payload'
        else:
            directory = root/'desktop'/relative
            meta = json.loads((directory/'package.json').read_text())
            if meta['version'] != package['version']:
                raise ValueError('installed frontend package version mismatch')
            files = local_notices(directory)
            if args.fetch:
                url = package['resolved']
                if not url.startswith('https://registry.npmjs.org/'):
                    raise ValueError('unsupported npm source')
                data = download(url, 64 * 1024 * 1024)
                entry['archive_sha256'] = digest(data)
                entry['archive_checked_notices_and_fonts'] = verify_npm_runtime(data, package['integrity'], directory)
            else:
                report['missing'].append({'frontend': relative, 'reason': 'exact registry archive check requires --fetch'})
            entry['installed_package_json'] = file_ref(directory/'package.json')
            entry['notices'] = {name: digest(data) for name, data in files.items()}
            if not files:
                report['missing'].append({'frontend': relative, 'reason': 'runtime notice missing'})
            frontend_items.append((f"{meta['name']}@{meta['version']}", files))
            if meta['name'].startswith('@fontsource/'):
                entry['font_assets'] = {str(p.relative_to(directory)): file_ref(p) for p in sorted(directory.rglob('*.woff*'))}
        report['frontend'].append(entry)
    component('frontend-dependencies', frontend_items)
    rust_doc = args.rust_sysroot/'share/doc/rust'
    toolchain_files = {'COPYRIGHT-library.html': (rust_doc/'COPYRIGHT-library.html').read_bytes()}
    toolchain_files.update({f'licenses/{p.name}': p.read_bytes() for p in sorted((rust_doc/'licenses').glob('*.txt'))})
    report['inputs']['rust_toolchain'] = file_ref(root/'rust-toolchain.toml')
    report['inputs']['rust_library_copyright'] = file_ref(rust_doc/'COPYRIGHT-library.html')
    component('rust-toolchain', [(args.rust_sysroot.name, toolchain_files)])
    component('adobe-dng-sdk', [('Adobe DNG SDK', {'LICENSE.txt': (args.sdk/'LICENSE.txt').read_bytes()})])
    vendor = root/'vendor/xmp_toolkit'
    component('xmp-toolkit', [('xmp_toolkit 1.12.1 with source-preservation changes', local_notices(vendor))])
    for name in NATIVE:
        directory = (args.brew/'opt'/name).resolve()
        files = local_notices(directory)
        if name == 'jpeg-turbo':
            extra = directory/'share/doc/libjpeg-turbo/README.ijg'
            files['README.ijg'] = extra.read_bytes()
        supplemental = []
        if name == 'aom' and args.fetch:
            recipe = (directory/'.brew/aom.rb').read_text()
            revision = re.search(r'revision: "([0-9a-f]{40})"', recipe)[1]
            url = f'https://aomedia.googlesource.com/aom/+/{revision}/PATENTS?format=TEXT'
            data = base64.b64decode(download(url, 1024 * 1024), validate=True)
            files['PATENTS'] = data
            supplemental.append({'url': url, 'sha256': digest(data), 'installed_revision': revision})
        if name == 'libvmaf' and args.fetch:
            recipe = (directory/'.brew/libvmaf.rb').read_text()
            url = re.search(r'^  url "(https://[^"]+)"', recipe, re.M)[1]
            checksum = re.search(r'^  sha256 "([0-9a-f]{64})"', recipe, re.M)[1]
            data = download(url, 64 * 1024 * 1024)
            if digest(data) != checksum:
                raise ValueError('VMAF source archive checksum mismatch')
            required = {'libvmaf/src/svm.h', 'libvmaf/src/feature/third_party/xiph/psnr_hvs.c'}
            with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
                for member in archive:
                    relative = member.name.split('/', 1)[-1]
                    if member.isfile() and relative in required:
                        source = archive.extractfile(member).read()
                        files[relative] = source
                        supplemental.append({'source_archive': url, 'archive_sha256': checksum,
                                             'path': relative, 'sha256': digest(source)})
            if not required <= files.keys():
                raise ValueError('missing exact VMAF embedded third-party notices')
        if name in {'aom', 'libvmaf'} and not args.fetch:
            report['missing'].append({'component': name, 'reason': 'embedded/patent notice retrieval requires --fetch'})
        libraries = {p.name for p in (directory/'lib').glob('*.dylib')}
        report['native'].append({'component': name, 'root': str(directory),
                                'receipt': file_ref(directory/'INSTALL_RECEIPT.json'),
                                'libraries': {p.name: file_ref(p.resolve()) for p in (directory/'lib').glob('*.dylib')},
                                'notices': {n: digest(b) for n,b in files.items()}, 'supplemental_sources': supplemental})
        component(name, [(str(directory), files)], libraries)
        if name == 'libraw':
            recipe = directory/'.brew/libraw.rb'
            recipe_text = recipe.read_text()
            source_url = re.search(r'^  url "(https://[^"]+)"', recipe_text, re.M)[1]
            source_sha = re.search(r'^  sha256 "([0-9a-f]{64})"', recipe_text, re.M)[1]
            if not args.fetch:
                report['missing'].append({'component': name, 'reason': 'exact corresponding source requires --fetch'})
            else:
                source_data = download(source_url, 64 * 1024 * 1024)
                if digest(source_data) != source_sha:
                    raise ValueError('LibRaw corresponding-source checksum mismatch')
                source_file = out/'libraw-source.tar.gz'
                source_file.write_bytes(source_data)
                recipe_file = out/'libraw-build-recipe.rb'
                recipe_file.write_bytes(recipe.read_bytes())
                manifest['components'][-1]['files'].extend([
                    {'path': source_file.name, 'sha256': source_sha},
                    {'path': recipe_file.name, 'sha256': digest(recipe_file.read_bytes())}])
                report['native'][-1]['included_source'] = {'url': source_url, 'sha256': source_sha,
                    'recipe': file_ref(recipe), 'basis': 'exact installed formula source and build recipe; included with library notices'}
    for pin in report['inputs'].values():
        if file_ref(pin['path']) != pin:
            raise ValueError('input changed during collection: ' + pin['path'])
    report['status'] = 'NOTICE_TEXT_COVERAGE_COMPLETE_PENDING_PACKAGE_REVIEW' if not report['missing'] else 'MISSING_NOTICES_HELD'
    (out/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    (out/'inventory.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps({'output':str(out),'status':report['status'],'missing':len(report['missing'])}), flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--checkout', type=Path, required=True)
    parser.add_argument('--sdk', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cargo-home', type=Path, default=Path.home()/'.cargo')
    parser.add_argument('--brew', type=Path, default=Path('/opt/homebrew'))
    parser.add_argument('--rust-sysroot', type=Path, required=True)
    parser.add_argument('--fetch', action='store_true')
    report = collect(parser.parse_args())
    raise SystemExit(bool(report['missing']))


if __name__ == '__main__':
    main()
