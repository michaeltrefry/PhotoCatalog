"""Independent codec/header readback. No photocatalog binaries or LCMS calls."""
from __future__ import annotations
import hashlib
import struct
import zlib
from pathlib import Path
import xml.etree.ElementTree as ET
import edit_reference as ref

METADATA_LIMIT = 16*1024*1024


def jpeg_metadata(stream, max_pixels):
    if stream.read(2) != b'\xff\xd8':
        raise ValueError('JPEG SOI')
    icc,standard,extended,exif = {},[],{},None
    total = 0
    dimensions = None
    while True:
        marker = stream.read(2)
        if len(marker)!=2 or marker[0]!=255:
            raise ValueError('JPEG marker')
        if marker[1] in (0xda,0xd9):
            break
        size = int.from_bytes(stream.read(2),'big')
        if size<2:
            raise ValueError('JPEG segment size')
        data = stream.read(size-2)
        if len(data)!=size-2:
            raise ValueError('JPEG truncated segment')
        total += len(data)
        if total>METADATA_LIMIT:
            raise ValueError('JPEG metadata admission')
        if marker[1] in (0xc0,0xc1,0xc2):
            if len(data)<6 or data[0]!=8:
                raise ValueError('JPEG SOF')
            height,width=struct.unpack('>HH',data[1:5])
            if not width or not height or width*height>max_pixels:
                raise ValueError('JPEG pixel admission before decode')
            dimensions=(width,height)
        if marker[1]==0xe2 and data.startswith(b'ICC_PROFILE\0'):
            sequence,count = data[12:14]
            if sequence in icc or not 1<=sequence<=count:
                raise ValueError('JPEG ICC sequence')
            icc[sequence] = (count,data[14:])
        if marker[1]==0xe1 and data.startswith(b'http://ns.adobe.com/xap/1.0/\0'):
            standard.append(data[29:])
        if marker[1]==0xe1 and data.startswith(b'http://ns.adobe.com/xmp/extension/\0'):
            part = data[35:]
            guid,full,offset = part[:32],int.from_bytes(part[32:36],'big'),int.from_bytes(part[36:40],'big')
            if full>METADATA_LIMIT or offset+len(part[40:])>full:
                raise ValueError('extended XMP byte bound')
            extended.setdefault(guid,[]).append((full,offset,part[40:]))
        if marker[1]==0xe1 and data.startswith(b'Exif\0\0'):
            if exif is not None:
                raise ValueError('duplicate EXIF')
            exif=data[6:]
    if dimensions is None:
        raise ValueError('JPEG dimensions unavailable')
    profiles = b''
    if icc:
        count=next(iter(icc.values()))[0]
        if set(icc)!=set(range(1,count+1)) or any(v[0]!=count for v in icc.values()):
            raise ValueError('incomplete ICC')
        profiles=b''.join(icc[i][1] for i in range(1,count+1))
    packets=list(standard)
    for guid,parts in extended.items():
        parts.sort(key=lambda p:p[1])
        position=0
        for full,offset,data in parts:
            if offset!=position or full!=parts[0][0]:
                raise ValueError('extended XMP gaps/overlap')
            position+=len(data)
        blob=b''.join(p[2] for p in parts)
        if position!=parts[0][0] or hashlib.md5(blob).hexdigest().upper().encode()!=guid:
            raise ValueError('extended XMP full hash')
        if not any(guid in packet for packet in standard):
            raise ValueError('extended XMP missing standard GUID link')
        packets.append(blob)
    return profiles,packets,exif


def inflate_limited(data, limit):
    decoder=zlib.decompressobj()
    output=decoder.decompress(data,limit+1)
    if len(output)>limit or decoder.unconsumed_tail or not decoder.eof or decoder.unused_data:
        raise ValueError('compressed metadata bounds/framing')
    return output


def png_metadata(stream, max_pixels):
    if stream.read(8)!=b'\x89PNG\r\n\x1a\n':
        raise ValueError('PNG signature')
    icc,packets,exif=b'',[],None
    while True:
        header=stream.read(8)
        if len(header)!=8:
            raise ValueError('PNG truncated chunk header')
        size,kind=struct.unpack('>I4s',header)
        if kind==b'IDAT':
            # Do not allocate compressed pixel chunks merely to inspect metadata.
            stream.seek(size+4,1)
            continue
        if size>METADATA_LIMIT:
            raise ValueError('PNG metadata chunk bound')
        data=stream.read(size)
        crc=stream.read(4)
        if len(data)!=size or len(crc)!=4 or zlib.crc32(kind+data)!=int.from_bytes(crc,'big'):
            raise ValueError('PNG metadata CRC')
        if kind==b'IHDR':
            if len(data)!=13:
                raise ValueError('PNG IHDR size')
            width,height=struct.unpack('>II',data[:8])
            if not width or not height or width*height>max_pixels:
                raise ValueError('PNG pixel admission before decode')
        if kind==b'iCCP':
            _,rest=data.split(b'\0',1)
            if not rest or rest[0]!=0 or icc:
                raise ValueError('PNG ICC encoding/duplicate')
            icc=inflate_limited(rest[1:],METADATA_LIMIT)
        elif kind==b'iTXt':
            keyword,rest=data.split(b'\0',1)
            compressed,method=rest[:2]
            _,_,text=rest[2:].split(b'\0',2)
            if method or compressed not in (0,1):
                raise ValueError('PNG text compression')
            if keyword==b'XML:com.adobe.xmp':
                packets.append(inflate_limited(text,METADATA_LIMIT) if compressed else text)
        elif kind==b'eXIf':
            exif=data
        elif kind==b'IEND':
            if size:
                raise ValueError('PNG IEND size')
            break
    return icc,packets,exif


def xmp_facts(packets):
    facts={}
    rdf='{http://www.w3.org/1999/02/22-rdf-syntax-ns#}'
    for packet in packets:
        root=ET.fromstring(packet)
        for description in root.iter(rdf+'Description'):
            for key,value in description.attrib.items():
                if key!=rdf+'about':
                    facts.setdefault(key,[]).append(value)
            for node in description:
                if len(node)==0:
                    facts.setdefault(node.tag,[]).append(node.text or '')
                else:
                    facts.setdefault(node.tag,[]).append([(i.tag,sorted(i.attrib.items()),i.text or '')
                                                        for i in node.iter() if i is not node])
    return facts


def read(path, max_pixels=32_000_000):
    import imagecodecs
    import tifffile
    np=ref.np_module()
    path=Path(path)
    # TIFF readback keeps float/uint16/alpha, unlike conversion through RGB8.
    with path.open('rb') as stream:
        signature=stream.read(8)
        stream.seek(0)
        if signature[:2] in (b'II',b'MM'):
            with tifffile.TiffFile(stream) as tf:
                page=tf.pages[0]
                if page.imagewidth*page.imagelength>max_pixels:
                    raise ValueError('TIFF pixel admission')
                tags=page.tags
                data=page.asarray()
                icc=bytes(tags[34675].value) if 34675 in tags else b''
                packets=[bytes(tags[700].value)] if 700 in tags else []
                metadata=dict(bits=page.bitspersample,sample_format=page.sampleformat,
                              extrasamples=list(page.extrasamples),orientation=tags[274].value if 274 in tags else None)
                return data,dict(format='tiff',icc=icc,packets=packets,metadata=metadata)
        if signature[:2]==b'\xff\xd8':
            icc,packets,exif=jpeg_metadata(stream,max_pixels)
            # jpeg8 decoder uses independent libjpeg, not Rust's image JPEG encoder.
            stream.seek(0)
            encoded=stream.read(512*1024*1024+1)
            if len(encoded)>512*1024*1024:
                raise ValueError('JPEG encoded admission')
            data=imagecodecs.jpeg8_decode(encoded)
            fmt='jpeg'
        elif signature==b'\x89PNG\r\n\x1a\n':
            icc,packets,exif=png_metadata(stream,max_pixels)
            stream.seek(0)
            encoded=stream.read(512*1024*1024+1)
            if len(encoded)>512*1024*1024:
                raise ValueError('PNG encoded admission')
            data=imagecodecs.png_decode(encoded)
            fmt='png'
        else:
            raise ValueError('unknown output signature')
    if data.shape[0]*data.shape[1]>max_pixels or not np.isfinite(data).all():
        raise ValueError('decoded pixels/nonfinite')
    return data,dict(format=fmt,icc=icc,packets=packets,exif=exif,
                    metadata=dict(bits=data.dtype.itemsize*8,sample_format='integer'))


def exif_tags(blob):
    """Read only root/Exif IFD scalar tags; reject invalid offsets/cycles."""
    if blob is None:
        return {}
    if len(blob)>METADATA_LIMIT or len(blob)<8 or blob[:2] not in (b'II',b'MM'):
        raise ValueError('EXIF framing')
    order='<' if blob[:2]==b'II' else '>'
    def unpack(fmt,at):
        size=struct.calcsize(order+fmt)
        if at<0 or at+size>len(blob):
            raise ValueError('EXIF offset')
        return struct.unpack_from(order+fmt,blob,at)
    if unpack('H',2)[0]!=42:
        raise ValueError('EXIF TIFF version')
    pending=[unpack('I',4)[0]]
    visited=set()
    tags={}
    sizes={1:1,2:1,3:2,4:4,5:8,7:1,9:4,10:8}
    while pending:
        offset=pending.pop()
        if offset in visited or len(visited)>=2:
            raise ValueError('EXIF cycle/additional directories')
        visited.add(offset)
        count=unpack('H',offset)[0]
        if count>64:
            raise ValueError('EXIF tag bound')
        for i in range(count):
            at=offset+2+12*i
            tag,kind,n=unpack('HHI',at)
            if kind not in sizes or n*sizes[kind]>METADATA_LIMIT:
                raise ValueError('EXIF tag type/size')
            size=n*sizes[kind]
            position=at+8 if size<=4 else unpack('I',at+8)[0]
            if position+size>len(blob):
                raise ValueError('EXIF value extent')
            if tag in tags:
                raise ValueError('duplicate EXIF tag')
            data=blob[position:position+size]
            if kind==2:
                value=data.rstrip(b'\0').decode('ascii')
            elif kind in (3,4,9):
                value=list(unpack({3:'H',4:'I',9:'i'}[kind]*n,position))
                value=value[0] if len(value)==1 else value
            elif kind in (5,10):
                value=unpack('II' if kind==5 else 'ii',position)
                if value[1]==0:
                    raise ValueError('EXIF zero denominator')
            else:
                value=data.hex()
            tags[tag]=value
            if tag==34665:
                pending.append(value)
        if unpack('I',offset+2+12*count)[0]:
            raise ValueError('unexpected EXIF thumbnail/source IFD')
    return tags


def verify(path, expected, spec, metadata, *, constant_jpeg=False):
    np=ref.np_module()
    data,info=read(path,max_pixels=expected.shape[0]*expected.shape[1])
    target=ref.output_pixels(expected,spec)
    if data.shape!=target.shape:
        raise ValueError('output shape/channel mismatch')
    if not info['icc']:
        raise ValueError('required output ICC absent')
    profile=info['icc']
    if profile[36:40]!=b'acsp' or profile[16:20]!=b'RGB ' or int.from_bytes(profile[:4],'big')!=len(profile):
        raise ValueError('ICC header')
    if spec['profile']['kind']=='icc' and profile!=bytes(spec['profile']['bytes']):
        raise ValueError('custom ICC bytes changed')
    error=np.abs(data.astype(np.float64)-target.astype(np.float64))
    if spec['format']['format']=='jpeg':
        if constant_jpeg:
            # DC-only interior constant fixture, quality90, explicit RGB conversion:
            # quantized DC reconstruction plus integer YCbCr rounding bound 3 LSB.
            passed=bool((error<=3).all())
            tolerance=3
        else:
            passed=None
            tolerance=None
    elif spec['format'].get('depth')=='float32':
        result=ref.compare(data,target,geometry_changed=True)
        passed=result['pass_']
        tolerance=dict(absolute=ref.GEOMETRY_ABS_TOL,relative=ref.REL_TOL)
    else:
        tolerance=4 if spec['format'].get('depth')=='sixteen' else 2
        passed=bool((error<=tolerance).all())
    facts=xmp_facts(info['packets'])
    if metadata.get('xmp'):
        expected_facts=xmp_facts([metadata['xmp'].encode()])
        for key,values in expected_facts.items():
            if facts.get(key)!=values:
                raise ValueError('XMP property values/order changed: '+key)
    elif info['packets']:
        raise ValueError('unexpected XMP when omission requested')
    if info['format']!='tiff':
        tags=exif_tags(info['exif'])
        if tags.get(274)!=1 or tags.get(256)!=data.shape[1] or tags.get(257)!=data.shape[0]:
            raise ValueError('EXIF physical orientation/dimensions')
        safe=metadata.get('exif',{})
        for key,tag in [('make',271),('model',272),('artist',315),('copyright',33432),
                        ('description',270),('lens',42036),('date_time_original',36867),('iso',34855)]:
            if safe.get(key) is not None and tags.get(tag)!=safe[key]:
                raise ValueError('safe EXIF value mismatch: '+key)
        for key,tag in [('exposure_time',33434),('f_number',33437),('focal_length',37386)]:
            if safe.get(key) is not None:
                expected_r=safe[key]
                actual_r=tags.get(tag)
                if actual_r is None or actual_r[0]*expected_r['denominator']!=actual_r[1]*expected_r['numerator']:
                    raise ValueError('safe EXIF rational mismatch: '+key)
    else:
        if info['metadata']['orientation']!=1:
            raise ValueError('TIFF physical orientation')
        actual_bits=info['metadata']['bits']
        expected_bits=32 if spec['format'].get('depth')=='float32' else 16 if spec['format'].get('depth')=='sixteen' else 8
        if actual_bits!=expected_bits:
            raise ValueError('TIFF encoded precision')
        if spec['format'].get('depth')=='float32' and int(info['metadata']['sample_format'])!=3:
            raise ValueError('TIFF floating SampleFormat')
        alpha=info['metadata']['extrasamples']
        if spec['alpha']['mode']=='preserve' and list(map(int,alpha))!=[2]:
            raise ValueError('TIFF straight alpha tag')
    with Path(path).open('rb') as stream:
        sha=hashlib.file_digest(stream,'sha256').hexdigest()
    return dict(path=str(path),sha256=sha,format=info['format'],shape=list(data.shape),
                dtype=str(data.dtype),icc_sha256=hashlib.sha256(profile).hexdigest(),
                xmp_sha256=[hashlib.sha256(p).hexdigest() for p in info['packets']],
                pixel_pass=passed,max_absolute_error=float(error.max()),tolerance=tolerance,
                jpeg_scope='constant fixture bound' if constant_jpeg else 'lossy errors reported; no arbitrary pixel threshold')
