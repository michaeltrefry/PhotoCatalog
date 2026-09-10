"""Immutable private helper/runtime admission before qualification child imports.

The external build receipt must hash this stdlib-only launcher itself. Launch it
with Python -I; it validates the complete frozen helper package before adding its
scripts directory to sys.path. Package collection is preparation, never timing.
"""
from __future__ import annotations
import argparse
import hashlib
import importlib
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import runpy
import sys

HELPERS=(
    'edit_binding','edit_campaign','edit_correctness_matrix','edit_disk_budget',
    'edit_fixtures','edit_qualification','edit_readback','edit_reference',
    'edit_statistics','edit_verify','preview_host',
)
DISTRIBUTIONS={'numpy':'2.5.3','tifffile':'2026.8.23','imagecodecs':'2026.8.16',
               'blake3':'1.0.9','psutil':'7.2.2'}
MAX_FILES=50000
MAX_FILE_BYTES=512*1024*1024


def file_hash(path):
    path=Path(path)
    if path.is_symlink() or not path.is_file():
        raise ValueError('binding requires ordinary non-symlink file')
    size=path.stat().st_size
    if size>MAX_FILE_BYTES:
        raise ValueError('binding file byte admission')
    h=hashlib.sha256()
    with path.open('rb') as stream:
        left=size
        while left:
            part=stream.read(min(65536,left))
            if not part: raise ValueError('binding file shrank')
            h.update(part)
            left-=len(part)
        if stream.read(1): raise ValueError('binding file grew')
    return h.hexdigest()


def regular_under(root,relative):
    root=Path(root).resolve(strict=True)
    path=root/relative
    if Path(relative).is_absolute() or '..' in Path(relative).parts or path.is_symlink():
        raise ValueError('binding path escape')
    actual=path.resolve(strict=True)
    if root not in actual.parents or actual!=path:
        raise ValueError('binding indirect path escape')
    return actual


def package_paths():
    return {f'scripts/{name}.py' for name in HELPERS}|{'benchmarks/observe_host.py','scripts/edit-requirements.txt'}


def collect_package(root):
    return {name:file_hash(regular_under(root,name)) for name in sorted(package_paths())}


def validate_package(value):
    root=Path(value['root']).resolve(strict=True)
    if set(value['files'])!=package_paths():
        raise ValueError('missing or extra frozen helper')
    for directory in ('scripts','benchmarks'):
        expected={Path(name).name for name in package_paths() if name.startswith(directory+'/')}
        with os.scandir(root/directory) as entries:
            actual_names=set()
            for entry in entries:
                if len(actual_names)>len(expected):
                    raise ValueError('unexpected frozen package entries')
                if not entry.is_file(follow_symlinks=False):
                    raise ValueError('frozen package contains non-file entry')
                actual_names.add(entry.name)
        if actual_names!=expected:
            raise ValueError('unbound file could shadow frozen imports')
    actual=collect_package(root)
    if actual!=value['files']:
        raise ValueError('frozen imported helper digest changed')
    return root


def distribution_files(name):
    distribution=importlib.metadata.distribution(name)
    if distribution.version!=DISTRIBUTIONS[name]:
        raise ValueError('qualification dependency version mismatch: '+name)
    root=Path(sys.prefix).resolve(strict=True)
    result={}
    for item in distribution.files or ():
        # Bytecode is derived and import may create it; canonical source and native
        # extensions, metadata/RECORD and data are all bound instead.
        if '__pycache__' in item.parts or str(item).endswith('.pyc'):
            continue
        path=Path(distribution.locate_file(item)).resolve(strict=True)
        if root not in path.parents:
            raise ValueError('dependency file outside selected environment')
        if path.is_file():
            result[str(path.relative_to(root))]=file_hash(path)
        if len(result)>MAX_FILES:
            raise ValueError('dependency file-count admission')
    if not result:
        raise ValueError('dependency lacks installed-file identity')
    return dict(version=distribution.version,files=result)


def runtime_identity():
    # Includes the executable bytes behind a venv symlink, exact Python build,
    # every installed dependency source/native/data file, and actual loaded
    # standard-library modules. Distribution collection precedes this snapshot.
    distributions={name:distribution_files(name) for name in DISTRIBUTIONS}
    standard={}
    stdlib=Path(sys.base_prefix).resolve(strict=True)
    for module in tuple(sys.modules.values()):
        origin=getattr(module,'__file__',None)
        if not origin: continue
        path=Path(origin).resolve(strict=True)
        if path.suffix=='.pyc' and path.with_suffix('.py').is_file():
            path=path.with_suffix('.py')
        if stdlib in path.parents and Path(sys.prefix).resolve() not in path.parents:
            standard[str(path)]=file_hash(path)
    return dict(executable=str(Path(sys.executable).resolve(strict=True)),
                executable_sha256=file_hash(Path(sys.executable).resolve(strict=True)),
                prefix=str(Path(sys.prefix).resolve()),base_prefix=str(stdlib),
                version=sys.version,cache_tag=sys.implementation.cache_tag,
                platform=sys.platform,distributions=distributions,stdlib=standard)


def validate_runtime(expected):
    # Compare exact distribution manifests independently of how many modules this
    # launcher happens to have imported; check every recorded stdlib file too.
    actual=runtime_identity()
    for field in ('executable','executable_sha256','prefix','base_prefix','version','cache_tag','platform','distributions'):
        if actual[field]!=expected[field]:
            raise ValueError('runtime binding mismatch: '+field)
    for path,digest in expected['stdlib'].items():
        if file_hash(path)!=digest:
            raise ValueError('standard-library identity changed')


def freeze_package(source,destination):
    source=Path(source).resolve(strict=True)
    destination=Path(destination)
    identities=collect_package(source)
    destination.mkdir() # exclusive; partial preparation is retained on failure
    for name,expected in identities.items():
        target=destination/name
        target.parent.mkdir(parents=True,exist_ok=True)
        with regular_under(source,name).open('rb') as incoming, target.open('xb') as outgoing:
            left=MAX_FILE_BYTES
            while part:=incoming.read(min(65536,left+1)):
                if len(part)>left: raise ValueError('helper grew during copy')
                outgoing.write(part)
                left-=len(part)
            outgoing.flush()
            os.fsync(outgoing.fileno())
        if file_hash(target)!=expected or file_hash(regular_under(source,name))!=expected:
            raise ValueError('helper changed during preparation')
    result=dict(root=str(destination.resolve(strict=True)),files=identities)
    validate_package(result)
    return result


def admit_imports(package):
    root=validate_package(package)
    scripts=root/'scripts'
    previous=sys.path[:]
    sys.path.insert(0,str(scripts))
    try:
        for name in HELPERS:
            spec=importlib.util.find_spec(name)
            if spec is None or Path(spec.origin).resolve()!=scripts/(name+'.py'):
                raise ValueError('helper resolution escaped frozen package: '+name)
            loaded=sys.modules.get(name)
            if loaded is not None and Path(loaded.__file__).resolve()!=scripts/(name+'.py'):
                raise ValueError('already imported unbound helper: '+name)
    except BaseException:
        sys.path[:]=previous
        raise
    return root


def verify_case_registry(expected,cases):
    if len(cases)!=529 or len({case['id'] for case in cases})!=529:
        raise ValueError('exact 529-case registry coverage required')
    canonical=json.dumps(cases,sort_keys=True,separators=(',',':'),allow_nan=False).encode()
    digest=hashlib.sha256(canonical).hexdigest()
    if expected!=digest:
        raise ValueError('case registry contents differ from frozen matrix')
    return digest


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binding',type=Path,required=True)
    parser.add_argument('--entry',choices=('edit_campaign','edit_verify','edit_fixtures'),required=True)
    parser.add_argument('arguments',nargs=argparse.REMAINDER)
    args=parser.parse_args()
    if not sys.flags.isolated:
        raise ValueError('frozen launcher requires Python -I')
    if args.binding.stat().st_size>16*1024*1024:
        raise ValueError('binding JSON byte admission')
    with args.binding.open('rb') as stream:
        data=stream.read(16*1024*1024+1)
    if len(data)>16*1024*1024: raise ValueError('binding JSON grew')
    value=json.loads(data)
    validate_runtime(value['python_runtime'])
    root=admit_imports(value['helper_package'])
    sys.dont_write_bytecode=True
    arguments=args.arguments[1:] if args.arguments[:1]==['--'] else args.arguments
    sys.argv=[str(root/'scripts'/(args.entry+'.py')),*arguments]
    runpy.run_module(args.entry,run_name='__main__')


if __name__=='__main__':
    main()
