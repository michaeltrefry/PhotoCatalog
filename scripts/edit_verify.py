"""Fresh-process verification of probe receipts and independent analytic pixels.

The coordinator supplies a byte-bound request. A successful check reports its
specific coverage; it never awards the whole S8 contract from receipt consistency.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import edit_reference as ref
import edit_readback
import edit_fixtures
import edit_statistics
import edit_artifacts
import edit_large_reference

MAX_JSON=256*1024
MAX_SAMPLES=16*1024*1024
PIXEL_PHASES={'correctness','kernel','full','support100mp','proxy_reference'}


def strict_json(data):
    def pairs(items):
        result={}
        for key,value in items:
            if key in result:
                raise ValueError('duplicate JSON key')
            result[key]=value
        return result
    def floating(value):
        result=float(value)
        if not math.isfinite(result):
            raise ValueError('nonfinite JSON number')
        return result
    def constant(value):
        raise ValueError('nonfinite JSON constant: '+value)
    return json.loads(data,object_pairs_hook=pairs,parse_float=floating,parse_constant=constant)


def read_json(path, limit=MAX_JSON):
    path=Path(path)
    if not path.is_file() or path.is_symlink() or path.stat().st_size>limit:
        raise ValueError('JSON file admission')
    with path.open('rb') as stream:
        data=stream.read(limit+1)
    if len(data)>limit:
        raise ValueError('JSON grew')
    return strict_json(data)


def digest(path, algorithm, limit):
    from blake3 import blake3
    path=Path(path)
    if path.is_symlink() or not path.is_file():
        raise ValueError('ordinary evidence file required')
    expected=path.stat().st_size
    if expected>limit:
        raise ValueError('evidence bytes exceed admission')
    h=blake3() if algorithm=='blake3' else hashlib.new(algorithm)
    with path.open('rb') as stream:
        left=expected
        while left:
            part=stream.read(min(left,65536))
            if not part:
                raise ValueError('evidence file shrank')
            h.update(part)
            left-=len(part)
        if stream.read(1):
            raise ValueError('evidence file grew')
    return h.hexdigest()


def owned(root,path):
    root=Path(root).resolve(strict=True)
    path=Path(path)
    if path.is_symlink():
        raise ValueError('evidence symlink')
    path=path.resolve(strict=True)
    if root not in path.parents:
        raise ValueError('artifact outside owned output')
    return path


def sample_records(stream, total_limit=MAX_SAMPLES, line_limit=MAX_JSON):
    # Bound every read before allocation, including growth after initial stat.
    used=0
    while True:
        line=stream.readline(min(line_limit,total_limit-used)+1)
        if not line:
            return
        used+=len(line)
        if used>total_limit or len(line)>line_limit:
            raise ValueError('sample byte admission exceeded')
        yield strict_json(line)


def observations(root):
    path=Path(root)/'samples.jsonl'
    if path.is_symlink() or not path.is_file() or path.stat().st_size>MAX_SAMPLES:
        raise ValueError('sample file admission')
    attempts=[]
    values=[]
    with path.open('rb') as stream:
        pending=None
        for value in sample_records(stream):
            if value.get('kind')=='attempt' and pending is None:
                pending=(value.get('recipe_index',0),value.get('iteration'))
                attempts.append(value)
            elif value.get('kind')=='observation' and pending is not None:
                if pending!=(value.get('recipe_index',0),value.get('iteration')):
                    raise ValueError('attempt/observation ordering mismatch')
                values.append(value)
                pending=None
            else:
                raise ValueError('unknown, unpaired or reordered sample record')
        if pending is not None:
            raise ValueError('incomplete attempted sample')
    return attempts,values


def sample_coverage(request,attempts,values):
    expected=edit_statistics.expected_identities(request)
    if not expected or len(attempts)!=len(expected) or len(values)!=len(expected):
        raise ValueError('partial/missing/extra sample coverage')
    for records in (attempts,values):
        seen=set()
        for record in records:
            key=edit_statistics.sample_identity(record,request)
            if any(type(v) is not int for v in key) or key not in expected or key in seen:
                raise ValueError('duplicate/out-of-range sample identity')
            seen.add(key)
        if seen!=expected:
            raise ValueError('sample Cartesian coverage mismatch')
    if [(v.get('recipe_index',0),v['iteration']) for v in attempts]!=[(v.get('recipe_index',0),v['iteration']) for v in values]:
        raise ValueError('attempt/observation identity mismatch')


def output_coverage(request,value):
    expected=request['outputs'] if request['phase'] in ('correctness','support100mp') else []
    artifacts=value.get('exports')
    if not isinstance(artifacts,list) or len(artifacts)!=len(expected):
        raise ValueError('missing/extra requested encodings')
    paths=[item.get('path') for item in artifacts]
    if any(not isinstance(path,str) for path in paths) or len(set(paths))!=len(paths):
        raise ValueError('duplicate or invalid output artifact')
    return zip(expected,artifacts,strict=True)


def overlap_proof(value,receipt,process_samples):
    before=value['live_workers_before']
    after=value['live_workers_after']
    if len(before)!=1 or before!=after:
        raise ValueError('save lacks stable kernel worker identity')
    identity=before[0]
    if (identity['parent_pid']!=receipt['probe_pid']
            or identity['pid'] not in value['owned_pids_before']
            or value['owned_pids_before']!=value['owned_pids_after']):
        raise ValueError('live worker is not the admitted direct child')
    anchors=[value[field]['unix_ns'] for field in ('live_before_at','started','finished','live_after_at')]
    if any(not isinstance(n,str) or not n.isdecimal() or len(n)>24 for n in anchors):
        raise ValueError('invalid kernel observation timestamp')
    times=[int(n) for n in anchors]
    if any(n<=0 for n in times) or times!=sorted(times):
        raise ValueError('kernel observations do not bracket durable save')
    created=identity['start_seconds']+identity['start_microseconds']/1_000_000
    if not math.isfinite(created) or created*1_000_000_000>times[0]:
        raise ValueError('impossible worker creation identity')
    matched=[]
    for sample in process_samples:
        for process in sample['processes']:
            if process['pid']==identity['pid'] and abs(process['create_time']-created)<=0.000001:
                if process['status'] in ('zombie','dead'):
                    raise ValueError('external observer saw terminated worker as live')
                matched.append(sample['at']['unix_ns'])
    if not matched:
        raise ValueError('kernel identity lacks independent OS observer corroboration')
    return dict(pid=identity['pid'],start_seconds=identity['start_seconds'],
                start_microseconds=identity['start_microseconds'],external_observations=len(matched))


def verify_case(root):
    root=Path(root)
    request=read_json(root/'request.json')
    receipt=read_json(root/'receipt.json')
    if not receipt.get('probe_complete') or receipt.get('qualification_complete') is not False:
        raise ValueError('incomplete or overclaiming probe receipt')
    for field in ('phase','fixture_id','operation','source_sha256','source_blake3'):
        if receipt[field]!=request[field]:
            raise ValueError('request/receipt identity mismatch: '+field)
    if digest(request['source'],'sha256',request['decode']['max_encoded_bytes'])!=request['source_sha256']:
        raise ValueError('owned source SHA256 differs')
    if digest(request['source'],'blake3',request['decode']['max_encoded_bytes'])!=request['source_blake3']:
        raise ValueError('owned source BLAKE3 differs')
    attempts,values=observations(root)
    per_recipe=request['warmups']+request['repetitions']
    pixel_phase=request['phase'] in PIXEL_PHASES|{'large_cancellation'}
    sample_coverage(request,attempts,values)
    process_samples=[]
    if request['phase'] in ('overlap_import','overlap_export'):
        if not root.name.endswith('-output'):
            raise ValueError('overlap requires frozen coordinator output namespace')
        telemetry=owned(root.parent,root.parent/root.name[:-7]/'processes.jsonl')
        with telemetry.open('rb') as stream:
            process_samples=list(sample_records(stream,total_limit=32*1024*1024))
    overlap=[]
    large=[]
    references=[]
    coverage={'sample_identity','source_hashes'}
    proofs=[]
    numerical=[]
    canonical=read_json(root/'recipes.json')
    if len(canonical)!=len(request['recipes']):raise ValueError('canonical recipe coverage')
    from blake3 import blake3
    for recipe,entry in zip(request['recipes'],canonical,strict=True):
        if strict_json(entry['canonical'])!=recipe or blake3(entry['canonical'].encode()).hexdigest()!=entry['digest']:
            raise ValueError('canonical recipe identity differs')
    for value in values:
        identity=(value.get('recipe_index',0),value['iteration'])
        if value.get('warmup')!=(identity[1]<request['warmups']):
            raise ValueError('warmup marker mismatch')
        if request['phase'] in ('overlap_import','overlap_export'):
            overlap.append(overlap_proof(value,receipt,process_samples))
            coverage.add('live_overlap')
        if request['phase']=='refusal':
            if not value.get('expected_refusal','').startswith('typed_'):
                raise ValueError('negative case lacks typed refusal')
            coverage.add('typed_refusal')
            continue
        if request['phase'] in ('warm_service','first_raw'):
            references.append(edit_artifacts.service_artifact(root,request,value))
            coverage.add('service_artifacts')
        if request['phase'] in ('export','export_correctness'):
            proof,dependencies=edit_artifacts.export_artifact(root,request,value)
            references.extend(dependencies)
            proofs.append(proof)
            coverage.update(('export_artifacts','encoded_pixels_metadata'))
        if pixel_phase:
            if value['recipe_digest']!=canonical[identity[0]]['digest']:
                raise ValueError('observed recipe digest differs')
            info=value['pixels']
            if info['nonfinite']!=0 or sum(info['alpha_zero_partial_opaque'])!=info['width']*info['height']:
                raise ValueError('pixel finite/alpha coverage')
            coverage.add('pixel_finite')
            raw=info.get('raw')
            actual=None
            if raw:
                path=owned(root,raw)
                size=info['width']*info['height']*16
                if path.stat().st_size!=size or digest(path,'blake3',size)!=info['rgba_f32le_blake3']:
                    raise ValueError('raw pixel artifact identity')
                np=ref.np_module()
                actual=np.memmap(path,mode='r',dtype='<f4',shape=(info['height'],info['width'],4))
            fixture=request['fixture_id']
            if fixture.startswith('analytic-'):
                if actual is None:
                    raise ValueError('analytic case lacks raw pixels')
                source=ref.fixture_to_linear(edit_fixtures.pixels(fixture))
                recipe=request['recipes'][identity[0]]
                expected_pixels=ref.render(source,recipe)
                proof=ref.compare(actual,expected_pixels,geometry_changed=bool(recipe['settings']['straighten_degrees']))
                coverage.add('analytic_pixels')
                numerical.append(dict(recipe_index=identity[0],**proof))
                if not proof['pass_']:
                    raise ValueError('independent analytic pixel mismatch')
            if request['phase'] in ('support100mp','large_cancellation'):
                if actual is None:raise ValueError('100MP reference pixels missing')
                large.append(edit_large_reference.verify_large(actual,request['recipes'][identity[0]],request['width'],request['height']))
                coverage.add('large_image_oracle')
            if request['phase']=='large_cancellation':
                if (value.get('typed_canceled') is not True or value.get('cancellation_polls')!=4
                    or value.get('completed_denoise_rows_before_cancel')!=2
                    or value.get('input_before_blake3')!=value.get('input_after_blake3')):
                    raise ValueError('large admitted cancellation/reuse proof differs')
                directory,expected,dependency=edit_artifacts.reference_case(root,request,
                    request['fixture_id']+'-admitted',request['recipes'][0])
                if expected['pixels']['rgba_f32le_blake3']!=info['rgba_f32le_blake3']:
                    raise ValueError('recovery differs from independently admitted combined output')
                references.append(dependency)
            if request['phase']=='proxy_reference':
                from edit_readback import read
                preview=value['preview_reference']
                path=owned(root,preview['path'])
                if digest(path,'blake3',8*1024*1024)!=preview['blake3']:
                    raise ValueError('proxy JPEG reference identity differs')
                decoded,descriptor=read(path,max_pixels=1600*1600,max_encoded_bytes=8*1024*1024,
                                        max_decoded_bytes=1600*1600*4)
                if descriptor['format']!='jpeg' or decoded.shape[:2]!=(preview['height'],preview['width']):
                    raise ValueError('proxy reference framing differs')
                coverage.add('proxy_artifacts')
            for specification,artifact in output_coverage(request,value):
                path=owned(root,artifact['path'])
                if digest(path,'blake3',request['encoded_extent'])!=artifact['blake3']:
                    raise ValueError('encoded artifact identity')
                if actual is None:
                    raise ValueError('export correctness lacks edited linear reference')
                proof=edit_readback.verify(path,actual,specification,request.get('metadata',{}),
                                          constant_jpeg=fixture=='analytic-flat',
                                          max_encoded_bytes=request['encoded_extent'],
                                          max_decoded_bytes=request['render']['max_allocation_bytes'])
                if proof['pixel_pass'] is False:
                    raise ValueError('independent encoded pixel mismatch')
                proofs.append(proof)
                coverage.add('encoded_pixels_metadata')
    return dict(version=1,verified=True,whole_story_qualified=False,request_sha256=digest(root/'request.json','sha256',MAX_JSON),
                receipt_sha256=digest(root/'receipt.json','sha256',MAX_JSON),
                samples_sha256=digest(root/'samples.jsonl','sha256',MAX_SAMPLES),
                sample_count=len(values),analytic=numerical,encoded=proofs,overlap=overlap,large=large,
                coverage=sorted(coverage),reference_cases=references,remaining=[])


def main():
    p=argparse.ArgumentParser()
    p.add_argument('--root',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    args=p.parse_args()
    result=None
    error=None
    try:
        result=verify_case(args.root)
    except Exception as exc:
        error=f'{type(exc).__name__}: {exc}'
    with args.output.open('x') as stream:
        json.dump(dict(complete=error is None,error=error,result=result),stream,indent=2,allow_nan=False)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    if error:
        raise SystemExit(error)

if __name__=='__main__':
    main()
