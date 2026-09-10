"""Independent codec/header readback. No photocatalog binaries or LCMS calls."""
from __future__ import annotations
import contextlib
import io
import math
import os
import stat
import hashlib
import struct
import zlib
from pathlib import Path
import xml.etree.ElementTree as ET
import edit_reference as ref

METADATA_LIMIT = 16*1024*1024


def jpeg_metadata(stream, max_pixels, metadata_limit=16*1024*1024):
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
        if total>metadata_limit:
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
    links=extended_guids(standard)
    if set(links)!={guid.decode('ascii') for guid in extended}:
        raise ValueError('extended XMP unbacked/extra GUID link')
    return profiles,packets,exif


def inflate_limited(data, limit):
    decoder=zlib.decompressobj()
    output=decoder.decompress(data,limit+1)
    if len(output)>limit or decoder.unconsumed_tail or not decoder.eof or decoder.unused_data:
        raise ValueError('compressed metadata bounds/framing')
    return output


def png_metadata(stream, max_pixels, metadata_limit=16*1024*1024):
    if stream.read(8)!=b'\x89PNG\r\n\x1a\n':
        raise ValueError('PNG signature')
    icc,packets,exif=b'',[],None
    used=0; ihdr=False
    while True:
        header=stream.read(8)
        if len(header)!=8:
            raise ValueError('PNG truncated chunk header')
        size,kind=struct.unpack('>I4s',header)
        if not ihdr and kind!=b'IHDR': raise ValueError('PNG IHDR must be first')
        if kind==b'IDAT':
            # Do not allocate compressed pixel chunks merely to inspect metadata.
            stream.seek(size+4,1)
            continue
        used+=size
        if used>metadata_limit:
            raise ValueError('PNG cumulative metadata chunk bound')
        data=stream.read(size)
        crc=stream.read(4)
        if len(data)!=size or len(crc)!=4 or zlib.crc32(kind+data)!=int.from_bytes(crc,'big'):
            raise ValueError('PNG metadata CRC')
        if kind==b'IHDR':
            if ihdr: raise ValueError('duplicate PNG IHDR')
            ihdr=True
            if len(data)!=13:
                raise ValueError('PNG IHDR size')
            width,height=struct.unpack('>II',data[:8])
            if not width or not height or width*height>max_pixels:
                raise ValueError('PNG pixel admission before decode')
        if kind==b'iCCP':
            _,rest=data.split(b'\0',1)
            if not rest or rest[0]!=0 or icc:
                raise ValueError('PNG ICC encoding/duplicate')
            icc=inflate_limited(rest[1:],metadata_limit)
            used+=len(icc)
            if used>metadata_limit: raise ValueError('PNG cumulative inflated metadata')
        elif kind==b'iTXt':
            keyword,rest=data.split(b'\0',1)
            compressed,method=rest[:2]
            _,_,text=rest[2:].split(b'\0',2)
            if method or compressed not in (0,1):
                raise ValueError('PNG text compression')
            if keyword==b'XML:com.adobe.xmp':
                text=inflate_limited(text,metadata_limit) if compressed else text
                if compressed: used+=len(text)
                if used>metadata_limit: raise ValueError('PNG cumulative inflated metadata')
                packets.append(text)
        elif kind==b'eXIf':
            if exif is not None: raise ValueError('duplicate PNG EXIF')
            exif=data
        elif kind==b'IEND':
            if size:
                raise ValueError('PNG IEND size')
            break
    return icc,packets,exif


RDF = '{http://www.w3.org/1999/02/22-rdf-syntax-ns#}'
XML = '{http://www.w3.org/XML/1998/namespace}'
EXTENDED = '{http://ns.adobe.com/xmp/note/}HasExtendedXMP'
ENCODED_LIMIT = 512*1024*1024
DECODED_LIMIT = 2*1024*1024*1024


def xml_tree(packet):
    packet.decode('utf-8')
    if len(packet)>METADATA_LIMIT or b'\0' in packet or b'<!DOCTYPE' in packet or b'<!ENTITY' in packet:
        raise ValueError('XMP byte/declaration admission')
    root=None; depth=0; count=0
    for event,node in ET.iterparse(io.BytesIO(packet),events=('start','end')):
        if event=='start':
            depth+=1; count+=1
            if root is None:root=node
            if count>100_000 or depth>64:raise ValueError('XMP node/depth admission')
        else:depth-=1

    return root


def xmp_facts(packets, *, transport_guids=()):
    """Canonical bounded RDF tree, preserving subject, qualifiers and hierarchy.

    Prefixes/compact properties are syntax; unordered property maps are sorted.
    Arrays retain their container type and item order. Unsupported RDF graph
    constructs fail explicitly, rather than being flattened into false equality.
    """
    def whitespace(text):
        return not text or not text.strip(' \t\r\n')
    def properties(nodes, attributes=()):
        result={}
        for key,literal in attributes:
            if key in result: raise ValueError('duplicate RDF property')
            result[key]=('literal',literal,())
        for node in nodes:
            if node.tag in result: raise ValueError('duplicate RDF property')
            result[node.tag]=value(node)
        return tuple(sorted(result.items()))
    def value(node):
        attrs=dict(node.attrib)
        for forbidden in (RDF+'datatype',RDF+'nodeID',RDF+'ID',XML+'base'):
            if forbidden in attrs: raise ValueError('unsupported RDF graph attribute')
        for child in node:
            if not whitespace(child.tail): raise ValueError('mixed RDF text')
        language=attrs.pop(XML+'lang',None)
        qualifier=() if language is None else ((XML+'lang',('literal',language.translate(str.maketrans('ABCDEFGHIJKLMNOPQRSTUVWXYZ','abcdefghijklmnopqrstuvwxyz')),())),)
        resource=attrs.pop(RDF+'resource',None)
        parse_type=attrs.pop(RDF+'parseType',None)
        if resource is not None:
            if list(node) or not whitespace(node.text) or parse_type or attrs:
                raise ValueError('unsupported RDF resource form')
            return ('resource',resource,qualifier)
        if parse_type is not None and parse_type!='Resource':
            raise ValueError('unsupported RDF parseType')
        if not list(node) and parse_type is None and not attrs:
            return ('literal',node.text or '',qualifier)
        if not whitespace(node.text): raise ValueError('mixed RDF property')
        children=list(node)
        if len(children)==1 and children[0].tag in (RDF+'Bag',RDF+'Seq',RDF+'Alt'):
            container=children[0]
            if attrs or parse_type or container.attrib or not whitespace(container.text):
                raise ValueError('unsupported RDF array qualifiers')
            if any(c.tag!=RDF+'li' or not whitespace(c.tail) for c in container):
                raise ValueError('RDF array item')
            return ('array',container.tag,tuple(value(c) for c in container),qualifier)
        if len(children)==1 and children[0].tag==RDF+'Description':
            description=children[0]
            if attrs or parse_type or not whitespace(description.text):
                raise ValueError('unsupported nested RDF description')
            attrs=dict(description.attrib)
            children=list(description)
        if any(key.startswith(RDF) or key.startswith(XML) for key in attrs):
            raise ValueError('unsupported RDF structure attribute')
        props=dict(properties(children,attrs.items()))
        if RDF+'value' in props:
            primary=props.pop(RDF+'value')
            index=3 if primary[0]=='array' else 2
            qualifiers=dict(primary[index])
            extra=dict(qualifier)
            if qualifiers.keys() & (props.keys()|extra.keys()) or props.keys() & extra.keys():
                raise ValueError('duplicate RDF qualifier')
            qualifiers.update(props); qualifiers.update(extra)
            return primary[:index]+(tuple(sorted(qualifiers.items())),)
        return ('struct',tuple(sorted(props.items())),qualifier)
    subjects={}
    seen_guids=[]
    total=0
    for packet in packets:
        total+=len(packet)
        if total>METADATA_LIMIT: raise ValueError('cumulative XMP bytes')
        root=xml_tree(packet)
        roots=list(root.iter(RDF+'RDF'))
        if len(roots)!=1: raise ValueError('exactly one RDF root required')
        rdf=roots[0]
        # Inherited language/base changes semantics even outside the RDF root.
        def ancestors(node):
            if node is rdf: return True
            for child in node:
                if ancestors(child):
                    if XML+'lang' in node.attrib or XML+'base' in node.attrib:
                        raise ValueError('inherited RDF language/base unsupported')
                    return True
            return False
        ancestors(root)
        if rdf.attrib or not whitespace(rdf.text): raise ValueError('RDF root attributes/text')
        for description in rdf:
            if description.tag!=RDF+'Description' or not whitespace(description.text) or not whitespace(description.tail):
                raise ValueError('RDF description form')
            attrs=dict(description.attrib)
            subject=attrs.pop(RDF+'about','')
            if any(k.startswith(RDF) or k.startswith(XML) for k in attrs):
                raise ValueError('unsupported RDF subject attributes')
            props=dict(properties(description,attrs.items()))
            transport=props.get(EXTENDED)
            if transport is not None and transport_guids:
                if transport[0]!='literal' or transport[2] or transport[1] not in transport_guids:
                    raise ValueError('extended XMP transport property')
                seen_guids.append(transport[1])
                del props[EXTENDED]
                if not props:continue  # Transport-only description asserts no application properties.
            target=subjects.setdefault(subject,{})
            if target.keys() & props.keys(): raise ValueError('duplicate cross-packet RDF property')
            target.update(props)
    if sorted(seen_guids)!=sorted(transport_guids):
        raise ValueError('extended XMP semantic GUID link')
    return {subject:tuple(sorted(props.items())) for subject,props in subjects.items()}


def icc_tags(profile):
    if len(profile)<132 or len(profile)>METADATA_LIMIT or profile[36:40]!=b'acsp' or profile[16:20]!=b'RGB ' or int.from_bytes(profile[:4],'big')!=len(profile):
        raise ValueError('ICC header')
    count=int.from_bytes(profile[128:132],'big')
    if count>128 or 132+count*12>len(profile): raise ValueError('ICC tag table')
    tags={}
    for i in range(count):
        at=132+12*i
        key=profile[at:at+4]
        offset,size=struct.unpack_from('>II',profile,at+4)
        if key in tags or offset<132+12*count or offset+size>len(profile) or size<8:
            raise ValueError('ICC tag extent/duplicate')
        tags[key]=profile[offset:offset+size]
    return tags


def verify_profile(profile, spec):
    tags=icc_tags(profile)
    if spec['kind']=='icc':
        if profile!=bytes(spec['bytes']): raise ValueError('custom ICC bytes changed')
        return
    if spec['kind'] not in ('srgb','linear_srgb'):
        raise ValueError('unknown expected ICC profile')
    if any(key[:3] in (b'A2B',b'B2A',b'D2B',b'B2D') for key in tags):
        raise ValueError('built-in profile unexpectedly contains a LUT')
    def xyz(key):
        data=tags.get(key,b'')
        if len(data)!=20 or data[:4]!=b'XYZ ': raise ValueError('ICC XYZ tag')
        return tuple(v/65536 for v in struct.unpack_from('>iii',data,8))
    # Independent chromaticity/Bradford construction, allowing only bounded
    # 15.16 tag quantization and published white-point rounding, never fit to runs.
    matrix=ref.srgb_matrix_d50()
    bound=8/65536
    for c,key in enumerate((b'rXYZ',b'gXYZ',b'bXYZ')):
        if any(abs(a-b)>bound for a,b in zip(xyz(key),matrix[:,c])):
            raise ValueError('built-in ICC primaries differ')
    if any(abs(a-b)>bound for a,b in zip(xyz(b'wtpt'),(.9642,1,.8249))):
        raise ValueError('built-in ICC white point differs')
    def curve(data,x):
        if data[:4]==b'curv':
            if len(data)<12: raise ValueError('ICC curve header')
            n=int.from_bytes(data[8:12],'big')
            if n>65536 or len(data)<12+2*n: raise ValueError('ICC curve count')
            if n==0:return x
            if n==1:return x**(int.from_bytes(data[12:14],'big')/256)
            position=x*(n-1); lower=min(n-2,int(position)); f=position-lower
            a,b=struct.unpack_from('>HH',data,12+2*lower)
            return (a*(1-f)+b*f)/65535
        if data[:4]!=b'para' or len(data)<12: raise ValueError('ICC TRC type')
        kind=int.from_bytes(data[8:10],'big'); counts={0:1,1:3,2:4,3:5,4:7}
        if kind not in counts or len(data)<12+4*counts[kind]: raise ValueError('ICC parametric curve')
        p=[v/65536 for v in struct.unpack_from('>'+'i'*counts[kind],data,12)]
        g=p[0]
        if kind==0:return x**g
        a,b=p[1:3]
        if a<=0: raise ValueError('ICC curve scale')
        if kind==1:return (a*x+b)**g if x>=-b/a else 0
        if kind==2:return (a*x+b)**g+p[3] if x>=-b/a else p[3]
        c,d=p[3:5]
        base=(a*x+b)**g if x>=d else c*x
        return base+(p[5] if x>=d else p[6]) if kind==4 else base
    for key in (b'rTRC',b'gTRC',b'bTRC'):
        for x in (0,.002,.01,.04,.04045,.1,.25,.5,.75,1):
            expected=x if spec['kind']=='linear_srgb' else x/12.92 if x<=.04045 else ((x+.055)/1.055)**2.4
            actual=curve(tags.get(key,b''),x)
            if not isinstance(actual,(int,float)) or not math.isfinite(actual) or abs(actual-expected)>bound:
                raise ValueError('built-in ICC transfer curve differs')


def tiff_fields(stream, length, metadata_limit=METADATA_LIMIT):
    """Bounded classic-TIFF root/Exif IFD scan before any codec allocation.

    This is the output format written by the frozen encoder; BigTIFF and linked
    image/thumbnail IFDs are explicit unsupported outputs, never silently ignored.
    """
    def take(at,n):
        if at<0 or n<0 or at+n>length: raise ValueError('TIFF field extent')
        stream.seek(at); data=stream.read(n)
        if len(data)!=n: raise ValueError('TIFF field truncation')
        return data
    header=take(0,8)
    if header[:2] not in (b'II',b'MM'): raise ValueError('TIFF byte order')
    order='<' if header[:2]==b'II' else '>'
    if struct.unpack_from(order+'H',header,2)[0]!=42: raise ValueError('unsupported TIFF version')
    pending=[struct.unpack_from(order+'I',header,4)[0]]
    visited=set(); fields={}; used=8
    sizes={1:1,2:1,3:2,4:4,5:8,6:1,7:1,8:2,9:4,10:8,11:4,12:8,13:4}
    while pending:
        at=pending.pop()
        if at in visited or len(visited)>=2: raise ValueError('TIFF IFD cycle/count')
        visited.add(at)
        count=struct.unpack(order+'H',take(at,2))[0]
        if count>128: raise ValueError('TIFF IFD tag admission')
        table=take(at+2,count*12+4); used+=len(table)+2
        if struct.unpack_from(order+'I',table,count*12)[0]:
            raise ValueError('unexpected additional image/thumbnail IFD')
        for i in range(count):
            tag,kind,n=struct.unpack_from(order+'HHI',table,i*12)
            if tag in fields or kind not in sizes: raise ValueError('TIFF duplicate/type')
            size=n*sizes[kind]; used+=size
            if used>metadata_limit: raise ValueError('cumulative TIFF metadata admission')
            data=table[i*12+8:i*12+8+size] if size<=4 else take(struct.unpack_from(order+'I',table,i*12+8)[0],size)
            fields[tag]=(kind,n,data,order)
            if tag==34665:
                if kind not in (4,13) or n!=1: raise ValueError('Exif IFD pointer type')
                pending.append(struct.unpack(order+'I',data)[0])
    return fields


def field_value(field):
    kind,n,data,order=field
    if kind==2:
        if not data.endswith(b'\0'): raise ValueError('unterminated EXIF ASCII')
        return data.rstrip(b'\0').decode('ascii')
    if kind in (3,4,8,9,13):
        values=struct.unpack(order+{3:'H',4:'I',8:'h',9:'i',13:'I'}[kind]*n,data)
        return values[0] if len(values)==1 else list(values)
    if kind in (5,10):
        if n!=1: raise ValueError('unexpected rational array')
        value=struct.unpack(order+('II' if kind==5 else 'ii'),data)
        if value[1]==0: raise ValueError('EXIF zero denominator')
        return value
    return data.hex()


def exif_tags(blob):
    if blob is None:return {}
    if len(blob)>METADATA_LIMIT: raise ValueError('EXIF byte admission')
    return {key:field_value(value) for key,value in tiff_fields(io.BytesIO(blob),len(blob)).items()}


@contextlib.contextmanager
def output_stream(path, maximum):
    before=os.lstat(path)
    if not stat.S_ISREG(before.st_mode) or before.st_size>maximum:
        raise ValueError('encoded output size/type admission')
    flags=os.O_RDONLY|getattr(os,'O_NONBLOCK',0)|getattr(os,'O_NOFOLLOW',0)
    with os.fdopen(os.open(path,flags),'rb') as stream:
        held=os.fstat(stream.fileno())
        if not stat.S_ISREG(held.st_mode) or (held.st_dev,held.st_ino,held.st_size)!=(before.st_dev,before.st_ino,before.st_size):
            raise ValueError('output changed during open')
        yield stream,held.st_size
        after=os.fstat(stream.fileno()); current=os.lstat(path)
        def stamp(s):return s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns,s.st_ctime_ns
        if stamp(after)!=stamp(held) or stamp(current)!=stamp(held):
            raise ValueError('output changed during readback')


COMPARISON_ROWS=8


def finite_rows(pixels):
    """No whole-image Boolean mask on an admitted large decoded/reference array."""
    np=ref.np_module()
    return all(np.isfinite(pixels[y:y+COMPARISON_ROWS]).all()
               for y in range(0,pixels.shape[0],COMPARISON_ROWS))


def compare_output_rows(data, expected, spec, *, constant_jpeg=False):
    """Preserve output conversion and max/pass math with bounded row temporaries.

    Pointwise profile/alpha/quantization work uses the unchanged reference oracle
    on eight rows. A genuine resize needs neighboring source rows, so the existing
    resize oracle runs once before these pointwise operations. Original-size and
    unchanged-size outputs retain no second full reference or quantized target.
    """
    np=ref.np_module()
    width,height=ref.output_dimensions(expected.shape[1],expected.shape[0],spec['size'])
    fmt=spec['format']['format']
    channels=3 if fmt=='jpeg' or spec['alpha']['mode']=='composite' else 4
    if data.shape!=(height,width,channels):
        raise ValueError('output shape/channel mismatch')
    basis=expected if (width,height)==(expected.shape[1],expected.shape[0]) else ref.resize(expected,width,height)
    pointwise={**spec,'size':{'mode':'original'}}
    depth=spec['format'].get('depth','eight')
    if fmt=='jpeg':
        if constant_jpeg and spec['format']['quality']!=90:
            raise ValueError('constant JPEG bound requires quality90')
        passed=True if constant_jpeg else None
        tolerance=3 if constant_jpeg else None
    elif depth=='float32':
        passed=True
        tolerance=dict(absolute=ref.GEOMETRY_ABS_TOL,relative=ref.REL_TOL)
    else:
        passed=True
        tolerance=4 if depth=='sixteen' else 2
    maximum=0.0
    for y in range(0,height,COMPARISON_ROWS):
        actual=data[y:y+COMPARISON_ROWS]
        target=ref.output_pixels(basis[y:y+COMPARISON_ROWS],pointwise)
        if actual.shape!=target.shape:
            raise ValueError('output shape/channel mismatch')
        # This is the same float64 difference as the former full-image casts.
        # Cast only the current actual block, then subtract/abs in place.
        error=actual.astype(np.float64)
        np.subtract(error,target,out=error)
        np.abs(error,out=error)
        if not np.isfinite(error).all():
            raise ValueError('nonfinite readback difference')
        maximum=max(maximum,float(error.max()))
        if fmt!='jpeg' and depth=='float32':
            # Keep the reference's original dtype promotion and exact tolerance
            # expression, independently of float64 reporting above.
            block_pass=ref.compare(actual,target,geometry_changed=True)['pass_']
            passed=passed and block_pass
        elif passed is not None:
            block_pass=bool((error<=tolerance).all())
            passed=passed and block_pass
        # Do not keep a previous block alive while constructing the next one.
        del error,target,actual
    return dict(pixel_pass=passed,max_absolute_error=maximum,tolerance=tolerance)


def read(path, max_pixels=32_000_000, *, max_encoded_bytes=ENCODED_LIMIT,
         max_decoded_bytes=DECODED_LIMIT, max_metadata_bytes=METADATA_LIMIT):
    import imagecodecs
    import tifffile
    np=ref.np_module()
    path=Path(path)
    if min(max_pixels,max_encoded_bytes,max_decoded_bytes,max_metadata_bytes)<=0:
        raise ValueError('positive readback budgets required')
    with output_stream(path,max_encoded_bytes) as (stream,length):
        signature=stream.read(8); stream.seek(0)
        if signature[:2] in (b'II',b'MM'):
            fields=tiff_fields(stream,length,max_metadata_bytes)
            scalar=lambda key,default=None:field_value(fields[key]) if key in fields else default
            width,height,samples=scalar(256),scalar(257),scalar(277,1)
            bits=scalar(258); sample_format=scalar(339,1)
            if not isinstance(width,int) or not isinstance(height,int) or min(width,height)<=0 or width*height>max_pixels or samples not in (3,4):
                raise ValueError('TIFF dimensions/channels admission')
            bits=bits if isinstance(bits,list) else [bits]*samples
            formats=sample_format if isinstance(sample_format,list) else [sample_format]*samples
            if len(bits)!=samples or len(set(bits))!=1 or bits[0] not in (8,16,32) or formats!=([3]*samples if bits[0]==32 else [1]*samples):
                raise ValueError('TIFF sample type/precision admission')
            if scalar(262)!=2 or scalar(284,1)!=1 or width*height*samples*(bits[0]//8)>max_decoded_bytes:
                raise ValueError('TIFF RGB/layout/allocation admission')
            stream.seek(0)
            # fdopen exposes an integer .name. Supply a display name while the
            # codec continues reading the same held descriptor, never reopening.
            with tifffile.TiffFile(stream,name=path.name) as tf:
                page=tf.pages[0]
                if tuple(page.shape)!=(height,width,samples) or page.dtype.itemsize!=bits[0]//8:
                    raise ValueError('TIFF independent descriptor mismatch')
                data=page.asarray(maxworkers=1)
            icc=fields[34675][2] if 34675 in fields else b''
            packets=[fields[700][2]] if 700 in fields else []
            info=dict(format='tiff',icc=icc,packets=packets,transport_guids=[],
                      tags={k:field_value(v) for k,v in fields.items() if k not in (34675,700)},
                      metadata=dict(bits=bits[0],sample_format=formats[0],extrasamples=scalar(338,[]),orientation=scalar(274)))
        else:
            if signature[:2]==b'\xff\xd8':
                icc,packets,exif=jpeg_metadata(stream,max_pixels,metadata_limit=max_metadata_bytes)
                fmt='jpeg'
                # Native JPEG8 has at most four channels. PNG header below binds
                # exact channels/depth; bound the worst JPEG output beforehand.
                required=max_pixels*4
            elif signature==b'\x89PNG\r\n\x1a\n':
                icc,packets,exif=png_metadata(stream,max_pixels,metadata_limit=max_metadata_bytes)
                fmt='png'
                stream.seek(16); width,height,depth,color=struct.unpack('>IIBB',stream.read(10))
                channels={2:3,6:4}.get(color)
                if channels is None or depth not in (8,16): raise ValueError('PNG RGB precision/channels')
                required=width*height*channels*(depth//8)
            else:raise ValueError('unknown output signature')
            if required>max_decoded_bytes: raise ValueError('decoded output allocation admission')
            stream.seek(0); encoded=stream.read(length+1)
            if len(encoded)!=length:raise ValueError('encoded output changed')
            data=imagecodecs.jpeg8_decode(encoded) if fmt=='jpeg' else imagecodecs.png_decode(encoded)
            del encoded
            info=dict(format=fmt,icc=icc,packets=packets,exif=exif,tags=exif_tags(exif),
                      transport_guids=extended_guids(packets) if fmt=='jpeg' else [],
                      metadata=dict(bits=data.dtype.itemsize*8,sample_format=1))
        stream.seek(0); digest=hashlib.sha256(); left=length
        while left:
            chunk=stream.read(min(left,65536))
            if not chunk: raise ValueError('output truncated while hashing')
            digest.update(chunk); left-=len(chunk)
        if stream.read(1): raise ValueError('output grew while hashing')
        info['sha256']=digest.hexdigest()
    if data.ndim!=3 or data.shape[2] not in (3,4) or data.shape[0]*data.shape[1]>max_pixels or data.nbytes>max_decoded_bytes or not finite_rows(data):
        raise ValueError('decoded shape/allocation/nonfinite')
    return data,info


def extended_guids(packets):
    values=[]
    for packet in packets:
        root=xml_tree(packet)
        for node in root.iter():
            if EXTENDED in node.attrib: values.append(node.attrib[EXTENDED])
            if node.tag==EXTENDED:
                if list(node) or node.attrib: raise ValueError('extended GUID literal')
                values.append(node.text or '')
    if any(len(value)!=32 or any(c not in '0123456789ABCDEF' for c in value) for value in values) or len(values)!=len(set(values)):
        raise ValueError('extended GUID identity')
    return values


def verify(path, expected, spec, metadata, *, constant_jpeg=False,
           max_encoded_bytes=ENCODED_LIMIT, max_decoded_bytes=DECODED_LIMIT,
           max_metadata_bytes=METADATA_LIMIT):
    np=ref.np_module()
    if expected.ndim!=3 or expected.shape[2]!=4 or not finite_rows(expected):
        raise ValueError('expected reference shape/nonfinite')
    width,height=ref.output_dimensions(expected.shape[1],expected.shape[0],spec['size'])
    if width*height*4*8>max_decoded_bytes:
        raise ValueError('reference output allocation admission')
    data,info=read(path,max_pixels=width*height,max_encoded_bytes=max_encoded_bytes,
                   max_decoded_bytes=max_decoded_bytes,max_metadata_bytes=max_metadata_bytes)
    fmt=spec['format']['format']
    if info['format']!=fmt: raise ValueError('encoded format differs from request')
    depth=spec['format'].get('depth','eight')
    expected_dtype={'eight':np.dtype('uint8'),'sixteen':np.dtype('uint16'),'float32':np.dtype('float32')}[depth]
    if data.dtype.kind!=expected_dtype.kind or data.dtype.itemsize!=expected_dtype.itemsize or info['metadata']['bits']!=expected_dtype.itemsize*8:
        raise ValueError('encoded precision/sample type differs from request')
    channels=3 if fmt=='jpeg' or spec['alpha']['mode']=='composite' else 4
    if data.shape!=(height,width,channels): raise ValueError('output shape/channel mismatch')
    if not info['icc']: raise ValueError('required output ICC absent')
    profile=info['icc']; verify_profile(profile,spec['profile'])
    comparison=compare_output_rows(data,expected,spec,constant_jpeg=constant_jpeg)
    facts=xmp_facts(info['packets'],transport_guids=info.get('transport_guids',()))
    if metadata.get('xmp'):
        expected_facts=xmp_facts([metadata['xmp'].encode()])
        if facts!=expected_facts: raise ValueError('full XMP subject/property semantics changed')
    elif info['packets']:
        raise ValueError('unexpected XMP when omission requested')
    tags=info['tags']
    for key,value in ((274,1),(256,data.shape[1]),(257,data.shape[0]),(40962,data.shape[1]),(40963,data.shape[0])):
        if tags.get(key)!=value: raise ValueError('EXIF physical orientation/dimensions')
    safe=metadata.get('exif',{})
    for key,tag in [('make',271),('model',272),('artist',315),('copyright',33432),
                    ('description',270),('lens',42036),('date_time_original',36867),('iso',34855)]:
        if tags.get(tag)!=safe.get(key): raise ValueError('safe EXIF value/omission mismatch: '+key)
    for key,tag in [('exposure_time',33434),('f_number',33437),('focal_length',37386)]:
        expected_r=safe.get(key); actual_r=tags.get(tag)
        if expected_r is None:
            if actual_r is not None: raise ValueError('unexpected safe EXIF rational: '+key)
        elif not isinstance(actual_r,(list,tuple)) or len(actual_r)!=2 or actual_r[0]*expected_r['denominator']!=actual_r[1]*expected_r['numerator']:
            raise ValueError('safe EXIF rational mismatch: '+key)
    if 37500 in tags: raise ValueError('copied source MakerNote is forbidden')
    if fmt=='tiff':
        if info['metadata']['sample_format']!=(3 if depth=='float32' else 1):
            raise ValueError('TIFF SampleFormat differs')
        alpha=info['metadata']['extrasamples']
        alpha=alpha if isinstance(alpha,list) else [alpha]
        if alpha!=([2] if spec['alpha']['mode']=='preserve' else []):
            raise ValueError('TIFF alpha association/channel tags')
    return dict(path=str(path),sha256=info['sha256'],format=info['format'],shape=list(data.shape),
                dtype=str(data.dtype),icc_sha256=hashlib.sha256(profile).hexdigest(),
                xmp_sha256=[hashlib.sha256(p).hexdigest() for p in info['packets']],
                **comparison,
                jpeg_scope='constant fixture bound' if constant_jpeg else 'lossy errors reported; no arbitrary pixel threshold')
