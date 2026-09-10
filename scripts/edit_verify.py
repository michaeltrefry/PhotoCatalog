"""Fresh-process verification of probe receipts and independent analytic pixels.

The coordinator supplies a byte-bound request. A successful check reports its
specific coverage; it never awards the whole S8 contract from receipt consistency.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import edit_reference as ref
import edit_readback
import edit_fixtures

MAX_JSON=256*1024
MAX_SAMPLES=16*1024*1024


def read_json(path, limit=MAX_JSON):
    path=Path(path)
    if not path.is_file() or path.is_symlink() or path.stat().st_size>limit:
        raise ValueError('JSON file admission')
    with path.open('rb') as stream:
        data=stream.read(limit+1)
    if len(data)>limit:
        raise ValueError('JSON grew')
    return json.loads(data)


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


def observations(root):
    path=Path(root)/'samples.jsonl'
    if path.stat().st_size>MAX_SAMPLES:
        raise ValueError('sample file byte bound')
    attempts=[]
    values=[]
    with path.open('rb') as stream:
        for line in stream:
            if len(line)>MAX_JSON:
                raise ValueError('sample line byte bound')
            value=json.loads(line)
            if value.get('kind')=='attempt':
                attempts.append(value)
            elif value.get('kind')=='observation':
                values.append(value)
            else:
                raise ValueError('unknown sample record')
    return attempts,values


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
    pixel_phase=request['phase'] in ('correctness','kernel','full','support100mp','proxy_reference')
    expected=per_recipe*len(request['recipes']) if pixel_phase else per_recipe
    if len(attempts)!=expected or len(values)!=expected:
        raise ValueError('partial/missing/extra sample coverage')
    seen=set()
    proofs=[]
    numerical=[]
    for value in values:
        identity=(value.get('recipe_index',0),value['iteration'])
        if identity in seen or not 0<=identity[1]<per_recipe:
            raise ValueError('duplicate/out-of-range sample identity')
        seen.add(identity)
        if value.get('warmup')!=(identity[1]<request['warmups']):
            raise ValueError('warmup marker mismatch')
        if request['phase']=='refusal':
            if not value.get('expected_refusal','').startswith('typed_'):
                raise ValueError('negative case lacks typed refusal')
            continue
        if pixel_phase:
            info=value['pixels']
            if info['nonfinite']!=0 or sum(info['alpha_zero_partial_opaque'])!=info['width']*info['height']:
                raise ValueError('pixel finite/alpha coverage')
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
                numerical.append(dict(recipe_index=identity[0],**proof))
                if not proof['pass_']:
                    raise ValueError('independent analytic pixel mismatch')
            for index,artifact in enumerate(value.get('exports',[])):
                path=owned(root,artifact['path'])
                if digest(path,'blake3',request['encoded_extent'])!=artifact['blake3']:
                    raise ValueError('encoded artifact identity')
                if actual is None:
                    raise ValueError('export correctness lacks edited linear reference')
                proof=edit_readback.verify(path,actual,request['outputs'][index],request.get('metadata',{}),
                                          constant_jpeg=fixture=='analytic-flat')
                if proof['pixel_pass'] is False:
                    raise ValueError('independent encoded pixel mismatch')
                proofs.append(proof)
    return dict(version=1,verified=True,whole_story_qualified=False,request_sha256=digest(root/'request.json','sha256',MAX_JSON),
                receipt_sha256=digest(root/'receipt.json','sha256',MAX_JSON),
                samples_sha256=digest(root/'samples.jsonl','sha256',MAX_SAMPLES),
                sample_count=len(values),analytic=numerical,encoded=proofs,
                remaining=['100MP streaming full/point oracle','service/export per-sample artifact oracle','overlap process intervals'])


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
