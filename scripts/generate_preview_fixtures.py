#!/usr/bin/env python3
"""Create six small protocol fixtures and independent prepared-RGB oracles.

Stdlib-only; no product image code, decoder, encoder, or resampler is imported.
Output directory must not exist. Fixture tests additionally cover negative float
RGB and all eight orientations beyond this matrix's orientation6 sample.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import random
import struct
import zlib

EDGES=(256,512,1600,2560)
def chunk(kind,data):
    return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
def png(width,height,depth,channels,pixels,orientation=1):
    rows=bytearray()
    for y in range(height):
        rows.append(0)
        samples=pixels[y*width*channels:(y+1)*width*channels]
        rows.extend(bytes(samples) if depth==8 else struct.pack('>'+str(len(samples))+'H',*samples))
    result=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',width,height,depth,2 if channels==3 else 6,0,0,0))+chunk(b'sRGB',b'\0')
    if orientation!=1:
        # Exactly one SHORT Orientation entry in little-endian TIFF.
        exif=b'II\x2a\0\x08\0\0\0\x01\0'+struct.pack('<HHIHHI',274,3,1,orientation,0,0)
        result+=chunk(b'eXIf',exif)
    return result+chunk(b'IDAT',zlib.compress(bytes(rows),9))+chunk(b'IEND',b'')
def linear(v):return v/12.92 if v<=0.04045 else ((v+0.055)/1.055)**2.4
def transfer(v):return 12.92*v if v<=0.0031308 else 1.055*v**(1/2.4)-0.055
def prepared(pixels,depth,channels):
    maximum=(1<<depth)-1
    result=bytearray()
    for i in range(0,len(pixels),channels):
        alpha=pixels[i+3]/maximum if channels==4 else 1.
        for v in pixels[i:i+3]:
            x=linear(v/maximum)*alpha+1-alpha
            result.append(math.floor(min(1,max(0,transfer(x)))*255+0.5))
    return result
def reduce_integer(rgb,width,height,edge):
    if max(width,height)<=edge:return width,height,bytes(rgb)
    if width!=height or width%edge:raise ValueError('fixture oracle intentionally accepts exact integer square reductions only')
    factor=width//edge;n=factor*factor;out=bytearray()
    for y in range(edge):
        for x in range(edge):
            for c in range(3):
                total=sum(rgb[((y*factor+j)*width+x*factor+k)*3+c] for j in range(factor) for k in range(factor))
                out.append((total+n//2)//n)
    return edge,edge,bytes(out)
def build(output):
    output=Path(output).resolve();output.mkdir(exist_ok=False)
    cases=[]
    values=[0,1,2651,32768,65534,65535]
    cases.append(('transfer',6,1,16,3,[c for v in values for c in (v,v,v)],1))
    cases.append(('alpha',3,1,16,4,[0,32768,65535,0,0,32768,65535,32768,0,32768,65535,65535],1))
    colors=[255,0,0,0,255,0,0,0,255,255,255,0,255,0,255,0,255,255]
    cases.append(('orientation',2,3,8,3,colors,6))
    block=[v for y in range(512) for x in range(512) for v in ([0,0,0] if (x+y)%2==0 else [255,255,255])]
    cases.append(('quantization',512,512,8,3,block,1))
    gradient=[v for y in range(512) for x in range(512) for v in (x//2,y//2,255 if x%32==0 else 0)]
    cases.append(('gradient',512,512,8,3,gradient,1))
    rng=random.Random(22841);noise=[rng.randrange(256) for _ in range(512*512*3)]
    cases.append(('noise',512,512,8,3,noise,1))
    manifest=[]
    for name,width,height,depth,channels,samples,orientation in cases:
        path=output/(name+'.png');data=png(width,height,depth,channels,samples,orientation);path.write_bytes(data)
        rgb=prepared(samples,depth,channels)
        if orientation==6:
            rgb=bytearray(v for i in [4,2,0,5,3,1] for v in rgb[i*3:i*3+3]);width,height=height,width
        oracle={}
        for edge in EDGES:
            w,h,data=reduce_integer(rgb,width,height,edge);target=output/f'{name}-{edge}.expected.rgb';target.write_bytes(data)
            oracle[str(edge)]={'path':str(target),'sha256':hashlib.sha256(data).hexdigest(),'width':w,'height':h}
        manifest.append({'id':'prep-'+name,'group':'preparation/'+name,'path':str(path),'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'width':width,'height':height,'kind':'preparation','oracle':oracle})
    (output/'fixtures.json').write_text(json.dumps({'version':1,'inputs':manifest},indent=2)+'\n')
if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('output');build(parser.parse_args().output)
