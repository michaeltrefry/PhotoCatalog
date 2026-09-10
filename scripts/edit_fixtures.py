"""Generated S8 sources; generation is explicit and requires a reviewed request.

No source generation occurs on import. TIFF rows stream; the 100MP source is not
an all-pixels Python allocation. These fixtures are CC0 mathematical constructions.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import edit_reference as ref

FIXTURES = {'analytic-signed-alpha': (16,12), 'analytic-impulse': (16,12),
            'analytic-noise': (32,24), 'analytic-flat': (48,32), 'analytic-metadata': (48,32),
            'support-100mp': (10000,10000), 'support-64mp': (8000,8000)}


def row(name, y):
    np = ref.np_module()
    w,h = FIXTURES[name]
    x = np.arange(w,dtype=np.int64)
    a = np.ones((w,4),dtype='<f4')
    if name in ('analytic-flat','analytic-metadata'):
        a[:] = [.18,.35,.7,.5]
    elif name == 'analytic-impulse':
        a[:,:3] = .18
        if y == h//2:
            a[w//2,:3] = [3,-.125,1.5]
        a[(x+y)%7==0,3] = .25
    elif name == 'analytic-noise':
        a[:,0] = ((x*17+y*29)%127)/128
        a[:,1] = ((x*31+y*11)%113)/128
        a[:,2] = ((x*7+y*43)%101)/128
        a[(x+y)%9==0,3] = .5
    else:
        # Periodic representable binary fractions include signed HDR and masks.
        a[:,0] = ((x+3*y)%17-2)/8
        a[:,1] = ((3*x+y)%13-1)/8
        a[:,2] = ((5*x+7*y)%19-3)/8
        a[:,3] = np.array([0,.25,.5,1],dtype='<f4')[(x+y)%4]
    return a


def pixels(name):
    if name in ('support-100mp','support-64mp'):
        raise ValueError('large source must stream, not stack')
    np = ref.np_module()
    return np.stack([row(name,y) for y in range(FIXTURES[name][1])]).astype(np.float64)


def generate(name, path):
    import tifffile
    w,h = FIXTURES[name]
    path = Path(path)
    tags=[(34675,'B',len(ref.matrix_profile()),ref.matrix_profile(),False)]
    if name=='analytic-metadata':
        import edit_correctness_matrix
        packet=edit_correctness_matrix.metadata(True)['xmp'].encode()
        tags.append((700,'B',len(packet),packet,False))
    # Exclusive create first; TiffWriter receives the owned file, never truncates a path.
    with path.open('xb') as stream:
        with tifffile.TiffWriter(stream, bigtiff=False) as writer:
            writer.write((row(name,y)[None,...] for y in range(h)), shape=(h,w,4),
                         dtype='<f4', photometric='rgb', extrasamples='unassalpha',
                         rowsperstrip=1, compression=None, metadata=None,
                         extratags=tags)
        stream.flush()
        os.fsync(stream.fileno())
    from blake3 import blake3
    sha=hashlib.sha256(); b3=blake3()
    with path.open('rb') as stream:
        while part:=stream.read(65536):
            sha.update(part);b3.update(part)
    digest=sha.hexdigest()
    return dict(id=name,path=str(path.resolve()),width=w,height=h,sha256=digest,blake3=b3.hexdigest(),
                bytes=path.stat().st_size,construction='edit_fixtures.row/v1',
                icc_sha256=hashlib.sha256(ref.matrix_profile()).hexdigest())


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--fixture',choices=FIXTURES,required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--receipt',type=Path,required=True)
    p.add_argument('--admitted',action='store_true',required=True)
    args = p.parse_args()
    if not args.output.is_absolute() or not args.receipt.is_absolute():
        raise ValueError('exclusive absolute output paths required')
    result = generate(args.fixture,args.output)
    with args.receipt.open('x') as stream:
        json.dump(result,stream,indent=2)
        stream.write('\n')

if __name__ == '__main__':
    main()
