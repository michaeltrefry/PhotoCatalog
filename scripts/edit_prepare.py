"""Exclusive untimed source-copy and generated-fixture preparation.

Requires a reviewed frozen build/runtime and explicit admission. Original sources
are opened read-only; no catalog/RAW decoding or native photo worker is launched.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import time
import psutil
import edit_binding
import edit_build_plan
import edit_campaign
import edit_disk_budget
import edit_fixtures
import edit_qualification as qualification
from edit_verify import digest,read_json
from preview_host import HostObservation,host_identity


def exclusive(path,value):
    with Path(path).open('x') as stream:
        json.dump(value,stream,indent=2,allow_nan=False);stream.write('\n')
        stream.flush();os.fsync(stream.fileno())


def source_copy(original,target,limit,expected_sha,free_reserve,deadline):
    from blake3 import blake3
    original=Path(original);target=Path(target)
    flags=os.O_RDONLY|getattr(os,'O_NONBLOCK',0)|getattr(os,'O_NOFOLLOW',0)
    descriptor=os.open(original,flags)
    with os.fdopen(descriptor,'rb') as incoming:
        before=os.fstat(incoming.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size>limit:
            raise ValueError('source copy requires admitted ordinary file')
        source_sha=hashlib.sha256();source_b3=blake3();remaining=before.st_size
        with target.open('xb') as outgoing:
            while remaining:
                if time.monotonic()>deadline or psutil.disk_usage(target.parent).free<free_reserve:
                    raise ValueError('copy deadline/free-space admission exhausted')
                data=incoming.read(min(65536,remaining))
                if not data:raise ValueError('original shrank during copy')
                outgoing.write(data);source_sha.update(data);source_b3.update(data);remaining-=len(data)
            if incoming.read(1):raise ValueError('original grew during copy')
            outgoing.flush();os.fsync(outgoing.fileno())
        after=os.fstat(incoming.fileno())
        fields=('st_dev','st_ino','st_size','st_mtime_ns','st_ctime_ns')
        if any(getattr(before,name)!=getattr(after,name) for name in fields):raise ValueError('original changed during copy')
        current=original.stat(follow_symlinks=False)
        if any(getattr(after,name)!=getattr(current,name) for name in fields):raise ValueError('original path changed during copy')
    if source_sha.hexdigest()!=expected_sha:raise ValueError('source differs from frozen original manifest')
    if digest(target,'sha256',limit)!=source_sha.hexdigest() or digest(target,'blake3',limit)!=source_b3.hexdigest():
        raise ValueError('new copy differs from held original bytes')
    return dict(original_path=str(original),path=str(target.resolve()),bytes=before.st_size,
                sha256=source_sha.hexdigest(),blake3=source_b3.hexdigest(),
                original_before={name:getattr(before,name) for name in fields},
                original_after={name:getattr(after,name) for name in fields})


def prepare(manifest_descriptor,build,root):
    root=Path(root)
    edit_binding.admit_imports(build['helper_package'])
    edit_binding.validate_runtime(build['python_runtime'])
    if digest(manifest_descriptor['path'],'sha256',qualification.MIB)!=manifest_descriptor['sha256']:
        raise ValueError('source cohort manifest differs')
    manifest=read_json(manifest_descriptor['path'],qualification.MIB)
    qualification.validate_manifest(manifest)
    funding=edit_disk_budget.budget(manifest)
    root.mkdir() # no resume/overwrite of a partial preparation
    exclusive(root/'start.json',dict(manifest=manifest_descriptor,build=build,funding=funding,
                                    deadline_seconds=3600,started=edit_campaign.anchor()))
    sources=[];copies=[];records=[];background=None;error=None
    deadline=time.monotonic()+3600
    try:
        if psutil.disk_usage(root).free<funding['minimum_free_bytes']:
            raise ValueError('preparation lacks the full final-campaign funding')
        (root/'sources').mkdir()
        exclusive(root/'host-identity.json',host_identity(root,[item['path'] for item in manifest['inputs']]))
        with HostObservation(root):
            for item in manifest['inputs']:
                directory=root/'sources'/item['id'];directory.mkdir()
                target=directory/('source'+Path(item['path']).suffix)
                copied=source_copy(item['path'],target,512*qualification.MIB,item['sha256'],funding['free_reserve_bytes'],deadline)
                copies.append(dict(id=item['id'],**copied))
                sources.append(dict(id=item['id'],width=item['width'],height=item['height'],**copied))
            # An actual second path forces a genuine new import job in overlap.
            item=next(item for item in manifest['inputs'] if item['id']=='private-X-T3-RAW')
            directory=root/'background-import';directory.mkdir()
            copied=source_copy(item['path'],directory/('source'+Path(item['path']).suffix),512*qualification.MIB,
                               item['sha256'],funding['free_reserve_bytes'],deadline)
            background=dict(fixture_id=item['id'],**copied)
            for fixture,(width,height) in edit_fixtures.FIXTURES.items():
                if time.monotonic()>deadline:raise ValueError('preparation total deadline')
                directory=root/'sources'/fixture;directory.mkdir()
                target=directory/'source.tiff'
                receipt=root/(fixture+'-fixture.json')
                command=edit_build_plan.python_command(build,'edit_fixtures',[
                    '--admitted','--fixture',fixture,'--output',target,'--receipt',receipt])
                limits=dict(deadline_seconds=600,process_rss_bytes=qualification.GIB,
                            group_rss_bytes=qualification.GIB,free_reserve_bytes=funding['free_reserve_bytes'])
                supervisor=root/('generate-'+fixture)
                edit_campaign.invoke(command,supervisor,limits,root)
                value=read_json(receipt)
                if value['id']!=fixture or value['path']!=str(target.resolve()) or (value['width'],value['height'])!=(width,height):
                    raise ValueError('generated fixture identity differs')
                if digest(target,'sha256',2*qualification.GIB)!=value['sha256'] or digest(target,'blake3',2*qualification.GIB)!=value['blake3']:
                    raise ValueError('generated bytes differ from receipt')
                descriptor=dict(path=str(receipt.resolve()),sha256=digest(receipt,'sha256',qualification.MIB))
                sources.append(dict(**value,generated_receipt=descriptor))
                records.append(dict(id=fixture,supervisor_path=str(supervisor/'result.json'),command=command,
                                    limits=limits,receipt_path=str(receipt),output=str(target)))
            exclusive(root/'copy-ledger.json',dict(complete=True,copies=copies,background=background,
                                                  source_write_policy='read-only held originals; create_new independent copies'))
    except BaseException as exc:error=f'{type(exc).__name__}: {exc}'
    result=dict(version=1,complete=error is None,error=error,root=str(root.resolve()),manifest=manifest_descriptor,
                sources=sources,preparation_records=records,background_copy=background,
                copies_observed=copies,finished=edit_campaign.anchor())
    if error is None:
        result['copy_receipt']=dict(path=str(root/'copy-ledger.json'),sha256=digest(root/'copy-ledger.json','sha256',16*qualification.MIB))
    exclusive(root/'preparation.json',result)
    if error:raise RuntimeError('preparation stopped; no retry: '+error)
    return result


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--manifest',type=Path,required=True)
    parser.add_argument('--manifest-sha256',required=True)
    parser.add_argument('--build',type=Path,required=True)
    parser.add_argument('--build-sha256',required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--admitted',action='store_true',required=True)
    args=parser.parse_args()
    if not args.output.is_absolute():raise ValueError('absolute owned preparation root required')
    if digest(args.build,'sha256',16*qualification.MIB)!=args.build_sha256:raise ValueError('frozen build descriptor differs')
    prepare(dict(path=str(args.manifest),sha256=args.manifest_sha256),read_json(args.build,16*qualification.MIB),args.output)

if __name__=='__main__':main()
