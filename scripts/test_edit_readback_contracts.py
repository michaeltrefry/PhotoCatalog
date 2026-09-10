"""Adversarial independent-oracle contracts; no production decoder/SDK calls."""
import io
import struct
import sys
import tempfile
import types
import unittest
import zlib
from pathlib import Path
from unittest.mock import MagicMock, patch
import edit_readback as rb
import edit_reference as ref


def packet(body, subject='subject'):
    return ('<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">'
            '<rdf:Description rdf:about="'+subject+'" xmlns:q="urn:test" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">'
            +body+'</rdf:Description></rdf:RDF>').encode()


def tiff(entries):
    table=bytearray(struct.pack('<H',len(entries)))
    tail=bytearray(); start=8+2+12*len(entries)+4
    for tag,kind,n,value in entries:
        table+=struct.pack('<HHI',tag,kind,n)
        if len(value)<=4: table+=value.ljust(4,b'\0')
        else:
            table+=struct.pack('<I',start+len(tail)); tail+=value
    return b'II*\0\x08\0\0\0'+table+b'\0'*4+tail


def builtin_profile(gamma=1):
    """Independent Decimal construction from sRGB xy/D65 and ICC D50.

    The custom matrix_profile intentionally has different fixed XYZ columns;
    it must not be used as a builtin-positive fixture. No oracle matrix solve
    or measured profile values are used to construct these builtin XYZ tags.
    """
    from decimal import Decimal as D, localcontext
    def multiply(a,b):
        return [[sum(x*y for x,y in zip(row,column)) for column in zip(*b)] for row in a]
    def inverse(a):
        cofactors=[]
        for i in range(3):
            row=[]
            for j in range(3):
                minor=[[a[y][x] for x in range(3) if x!=j] for y in range(3) if y!=i]
                row.append((minor[0][0]*minor[1][1]-minor[0][1]*minor[1][0])*(-1)**(i+j))
            cofactors.append(row)
        determinant=sum(a[0][j]*cofactors[0][j] for j in range(3))
        return [[cofactors[j][i]/determinant for j in range(3)] for i in range(3)]
    def diagonal(values):return [[v if i==j else D(0) for j in range(3)] for i,v in enumerate(values)]
    with localcontext() as context:
        context.prec=50
        xy=[(D('.64'),D('.33')),(D('.30'),D('.60')),(D('.15'),D('.06'))]
        primary=[list(row) for row in zip(*[(x/y,D(1),(1-x-y)/y) for x,y in xy])]
        white=[[D('.3127')/D('.3290')],[D(1)],[(1-D('.3127')-D('.3290'))/D('.3290')]]
        d50=[[D('.9642')],[D(1)],[D('.8249')]]
        basis=[[D(v) for v in row] for row in (('.8951','.2664','-.1614'),('-.7502','1.7135','.0367'),('.0389','-.0685','1.0296'))]
        scales=[row[0] for row in multiply(inverse(primary),white)]
        source=multiply(basis,white); target=multiply(basis,d50)
        adaptation=multiply(multiply(inverse(basis),diagonal([target[i][0]/source[i][0] for i in range(3)])),basis)
        columns=multiply(adaptation,multiply(primary,diagonal(scales)))
        profile=bytearray(ref.matrix_profile(gamma))
        count=int.from_bytes(profile[128:132],'big')
        for i in range(count):
            at=132+12*i; key=bytes(profile[at:at+4])
            if key in (b'rXYZ',b'gXYZ',b'bXYZ'):
                channel=(b'rXYZ',b'gXYZ',b'bXYZ').index(key)
                offset=int.from_bytes(profile[at+4:at+8],'big')
                profile[offset+8:offset+20]=b''.join(struct.pack('>i',round(columns[row][channel]*65536)) for row in range(3))
        return bytes(profile)


class FramingContracts(unittest.TestCase):
    def test_full_rdf_subject_qualifier_nesting_and_extra_properties(self):
        original=packet('<q:p xml:lang="en">text</q:p>')
        for bad in [packet('<q:p xml:lang="fr">text</q:p>'),
                    packet('<q:p xml:lang="en">text</q:p>','other'),
                    packet('<q:p xml:lang="en">text</q:p><crs:Exposure2012>1</crs:Exposure2012>')]:
            self.assertNotEqual(rb.xmp_facts([original]),rb.xmp_facts([bad]))
        nested=packet('<q:p rdf:parseType="Resource"><q:s rdf:parseType="Resource"><q:a>1</q:a></q:s><q:b>2</q:b></q:p>')
        moved=packet('<q:p rdf:parseType="Resource"><q:s rdf:parseType="Resource"><q:a>1</q:a><q:b>2</q:b></q:s></q:p>')
        self.assertNotEqual(rb.xmp_facts([nested]),rb.xmp_facts([moved]))
        qualified=packet('<q:p rdf:parseType="Resource"><rdf:value xml:lang="en">text</rdf:value></q:p>')
        self.assertEqual(rb.xmp_facts([original]),rb.xmp_facts([qualified]))
        compact=packet('').replace(b'xmlns:q="urn:test"',b'xmlns:q="urn:test" q:p="text"')
        self.assertEqual(rb.xmp_facts([compact]),rb.xmp_facts([packet('<q:p>text</q:p>')]))

    def test_qualified_unknown_values_and_array_order_are_not_flattened(self):
        a=packet('<q:p rdf:parseType="Resource"><rdf:value>v</rdf:value><q:unit>m</q:unit></q:p>')
        b=a.replace(b'>m<',b'>s<')
        self.assertNotEqual(rb.xmp_facts([a]),rb.xmp_facts([b]))
        a=packet('<q:p><rdf:Seq><rdf:li>1</rdf:li><rdf:li>2</rdf:li></rdf:Seq></q:p>')
        b=a.replace(b'>1<',b'>3<').replace(b'>2<',b'>1<').replace(b'>3<',b'>2<')
        self.assertNotEqual(rb.xmp_facts([a]),rb.xmp_facts([b]))
        for extra in (' rdf:datatype="urn:type"',' rdf:nodeID="a"'):
            with self.assertRaises(ValueError):rb.xmp_facts([packet('<q:p'+extra+'>x</q:p>')])

    def test_extended_xmp_requires_exact_semantic_guid_and_full_payload(self):
        import hashlib
        extended=packet('<q:p>retained</q:p>')
        guid=hashlib.md5(extended).hexdigest().upper()
        standard=packet('<n:HasExtendedXMP xmlns:n="http://ns.adobe.com/xmp/note/">'+guid+'</n:HasExtendedXMP>')
        def segment(marker,data):return bytes((255,marker))+struct.pack('>H',len(data)+2)+data
        sof=segment(0xc0,b'\x08'+struct.pack('>HH',1,1)+b'\x03')
        prefix=b'http://ns.adobe.com/xmp/extension/\0'
        def jpeg(main):
            return (b'\xff\xd8'+sof+segment(0xe1,b'http://ns.adobe.com/xap/1.0/\0'+main)
                    +segment(0xe1,prefix+guid.encode()+struct.pack('>II',len(extended),0)+extended)+b'\xff\xda')
        _,packets,_=rb.jpeg_metadata(io.BytesIO(jpeg(standard)),1)
        self.assertEqual(rb.xmp_facts(packets,transport_guids=[guid]),rb.xmp_facts([extended]))
        # Mere text occurrence must not stand in for the actual namespace property.
        with self.assertRaisesRegex(ValueError,'GUID link'):
            rb.jpeg_metadata(io.BytesIO(jpeg(packet('<q:comment>'+guid+'</q:comment>'))),1)

    def test_png_inflated_metadata_has_cumulative_budget(self):
        def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
        header=chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,6,0,0,0))
        text=b'XML:com.adobe.xmp\0\1\0\0\0'+zlib.compress(b'x'*90)
        one=b'\x89PNG\r\n\x1a\n'+header+chunk(b'iTXt',text)
        rb.png_metadata(io.BytesIO(one+chunk(b'IEND',b'')),1,metadata_limit=200)
        with self.assertRaisesRegex(ValueError,'cumulative'):
            rb.png_metadata(io.BytesIO(one+chunk(b'iTXt',text)+chunk(b'IEND',b'')),1,metadata_limit=200)

    def test_tiff_rejects_large_channels_before_codec_or_pixel_allocation(self):
        data=tiff([(256,4,1,struct.pack('<I',1)),(257,4,1,struct.pack('<I',1)),
                   (277,3,1,struct.pack('<H',65535)),(258,3,1,struct.pack('<H',8))])
        codec=MagicMock()
        with tempfile.TemporaryDirectory() as root:
            path=Path(root)/'output.tif'; path.write_bytes(data)
            with patch.dict(sys.modules,{'imagecodecs':types.SimpleNamespace(),'tifffile':codec}),patch.object(ref,'np_module',return_value=MagicMock()):
                with self.assertRaisesRegex(ValueError,'channels admission'):rb.read(path,max_pixels=1)
            codec.TiffFile.assert_not_called()

    def test_tiff_metadata_extent_and_output_encoded_admission(self):
        data=tiff([(315,2,7,b'artist\0')])
        self.assertEqual(rb.exif_tags(data)[315],'artist')
        with self.assertRaisesRegex(ValueError,'cumulative'):
            rb.tiff_fields(io.BytesIO(data),len(data),metadata_limit=10)
        with tempfile.TemporaryDirectory() as root:
            path=Path(root)/'large'; path.write_bytes(b'x'*11)
            with self.assertRaisesRegex(ValueError,'size/type'):
                with rb.output_stream(path,10):self.fail('oversized output opened for inspection')


class NumericContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        try:cls.np=ref.np_module()
        except ImportError:raise unittest.SkipTest('scientific environment unavailable; not acceptance')

    def test_tiff_reads_held_descriptor_at_each_supported_precision(self):
        import tifffile
        np=self.np
        for dtype in (np.uint8,np.uint16,np.float32):
            with self.subTest(dtype=dtype),tempfile.TemporaryDirectory() as root:
                path=Path(root)/'output.tiff'
                expected=np.arange(24,dtype=dtype).reshape(2,3,4)
                tifffile.imwrite(path,expected,photometric='rgb',extrasamples='unassalpha',metadata=None)
                before=path.read_bytes()
                actual,info=rb.read(path,max_pixels=6,max_decoded_bytes=expected.nbytes)
                np.testing.assert_array_equal(actual,expected)
                self.assertEqual(actual.dtype,expected.dtype)
                self.assertEqual(info['metadata']['bits'],expected.dtype.itemsize*8)
                self.assertEqual(path.read_bytes(),before)

    def test_expected_nonfinite_cannot_pass_reference_comparison(self):
        np=self.np
        for invalid in (float('nan'),float('inf'),-float('inf')):
            with self.assertRaises(ValueError):ref.compare(np.zeros((1,1,4)),np.full((1,1,4),invalid))

    def test_builtin_icc_rejects_wrong_transfer_or_primaries(self):
        linear=builtin_profile(1)
        rb.verify_profile(linear,{'kind':'linear_srgb'})
        custom=ref.matrix_profile(1)
        rb.verify_profile(custom,{'kind':'icc','bytes':list(custom)})
        with self.assertRaisesRegex(ValueError,'primaries'):
            rb.verify_profile(custom,{'kind':'linear_srgb'})
        tags=rb.icc_tags(linear)
        curve=b'para'+b'\0'*4+struct.pack('>HH',3,0)+b''.join(struct.pack('>i',round(v*65536)) for v in (2.4,1/1.055,.055/1.055,1/12.92,.04045))
        for channel in b'rgb':tags[bytes([channel])+b'TRC']=curve
        table=bytearray(struct.pack('>I',len(tags))); body=bytearray(); start=132+12*len(tags)
        for key,data in sorted(tags.items()):
            table+=key+struct.pack('>II',start+len(body),len(data))
            body+=data+b'\0'*((-len(data))%4)
        srgb=bytearray(linear[:128])+table+body
        srgb[:4]=struct.pack('>I',len(srgb))
        rb.verify_profile(bytes(srgb),{'kind':'srgb'})

        with self.assertRaisesRegex(ValueError,'transfer'):
            rb.verify_profile(linear,{'kind':'srgb'})
        with self.assertRaisesRegex(ValueError,'transfer'):
            rb.verify_profile(builtin_profile(2),{'kind':'linear_srgb'})
        bad=bytearray(linear)
        count=int.from_bytes(bad[128:132],'big')
        for i in range(count):
            at=132+i*12
            if bad[at:at+4]==b'rXYZ':
                offset=int.from_bytes(bad[at+4:at+8],'big');bad[offset+8:offset+12]=struct.pack('>i',32768)
        with self.assertRaisesRegex(ValueError,'primaries'):
            rb.verify_profile(bytes(bad),{'kind':'linear_srgb'})

    def spec(self,fmt='png',depth='eight',composite=False):
        return dict(format={'format':fmt,'depth':depth},profile={'kind':'linear_srgb'},
                    size={'mode':'original'},alpha={'mode':'composite','linear_rgb':[1,1,1]} if composite else {'mode':'preserve'})

    def info(self,fmt='png',bits=8,width=1,height=1,composite=False):
        return dict(format=fmt,icc=builtin_profile(),packets=[],transport_guids=[],sha256='0'*64,
                    tags={274:1,256:width,257:height,40962:width,40963:height},
                    metadata={'bits':bits,'sample_format':3 if bits==32 else 1,'extrasamples':[] if composite else [2]})

    def test_format_and_png_depth_rejected_even_when_zero_pixels_match(self):
        np=self.np; expected=np.zeros((1,1,4)); data=np.zeros((1,1,4),dtype=np.uint8)
        with patch.object(rb,'read',return_value=(data,self.info('tiff'))):
            with self.assertRaisesRegex(ValueError,'format differs'):rb.verify('unused',expected,self.spec(),{})
        with patch.object(rb,'read',return_value=(data.astype(np.uint16),self.info(bits=16))):
            with self.assertRaisesRegex(ValueError,'precision'):rb.verify('unused',expected,self.spec(),{})

    def test_upscale_admits_actual_target_and_float_composite_has_three_channels(self):
        np=self.np; expected=np.zeros((1,1,4)); spec=self.spec()
        spec['size']={'mode':'fit','width':2,'height':2,'allow_upscale':True}
        with patch.object(rb,'read',return_value=(np.zeros((2,2,4),dtype=np.uint8),self.info(width=2,height=2))) as reader:
            self.assertTrue(rb.verify('unused',expected,spec,{})['pixel_pass'])
            self.assertEqual(reader.call_args.kwargs['max_pixels'],4)
        spec=self.spec('tiff','float32',True)
        expected[...,:3]=[2,-.25,.5];expected[...,3]=.5
        target=ref.output_pixels(expected,spec)
        self.assertEqual(target.shape,(1,1,3))
        with patch.object(rb,'read',return_value=(target.astype(np.float32),self.info('tiff',32,composite=True))):
            self.assertTrue(rb.verify('unused',expected,spec,{})['pixel_pass'])

    def test_tiff_safe_exif_values_and_omission_are_checked(self):
        np=self.np; expected=np.zeros((1,1,4)); info=self.info('tiff');info['tags'][271]='selected'
        with patch.object(rb,'read',return_value=(np.zeros((1,1,4),dtype=np.uint8),info)):
            self.assertTrue(rb.verify('unused',expected,self.spec('tiff'),{'exif':{'make':'selected'}})['pixel_pass'])
            for metadata in ({},{'exif':{'make':'wrong'}}):
                with self.assertRaisesRegex(ValueError,'safe EXIF'):rb.verify('unused',expected,self.spec('tiff'),metadata)

    def test_verify_bounds_conversion_casts_and_finite_masks_to_eight_rows(self):
        np=self.np
        expected=np.zeros((17,3,4))
        class BoundedCast(np.ndarray):
            def astype(self,*args,**kwargs):
                if self.ndim==3 and self.shape[0]>8:
                    raise AssertionError('whole-image comparison cast')
                return super().astype(*args,**kwargs)
        data=np.zeros((17,3,4),dtype=np.uint8).view(BoundedCast)
        data[7,0,0]=2; data[8,0,0]=3; data[16,0,0]=9
        output_pixels=ref.output_pixels; isfinite=np.isfinite
        converted=[]
        def bounded_output(pixels,spec):
            self.assertLessEqual(pixels.shape[0],8,'whole-image output conversion')
            converted.append(pixels.shape[0])
            return output_pixels(pixels,spec)
        def bounded_finite(pixels,*args,**kwargs):
            if getattr(pixels,'ndim',0)==3:
                self.assertLessEqual(pixels.shape[0],8,'whole-image finite mask')
            return isfinite(pixels,*args,**kwargs)
        with patch.object(rb,'read',return_value=(data,self.info(width=3,height=17))), \
                patch.object(ref,'output_pixels',side_effect=bounded_output), \
                patch.object(np,'isfinite',side_effect=bounded_finite):
            result=rb.verify('unused',expected,self.spec(),{})
        self.assertFalse(result['pixel_pass'])
        self.assertEqual(result['max_absolute_error'],9)
        self.assertEqual(converted,[8,8,1])

    def test_integer_tolerance_and_negative_differences_across_row_boundary(self):
        np=self.np
        for depth,dtype,maximum,tolerance in [('eight',np.uint8,255,2),('sixteen',np.uint16,65535,4)]:
            with self.subTest(depth=depth):
                expected=np.zeros((17,2,4)); expected[16]=1
                data=np.zeros((17,2,4),dtype=dtype); data[16]=maximum
                data[7,0,0]=tolerance
                data[16,0,1]=maximum-tolerance
                good=rb.compare_output_rows(data,expected,self.spec(depth=depth))
                self.assertTrue(good['pixel_pass'])
                self.assertEqual(good['max_absolute_error'],tolerance)
                data[8,0,0]=tolerance+1
                bad=rb.compare_output_rows(data,expected,self.spec(depth=depth))
                self.assertFalse(bad['pixel_pass'])
                self.assertEqual(bad['max_absolute_error'],tolerance+1)

    def test_float_relative_tolerance_and_global_max_match_full_reference(self):
        np=self.np
        expected=np.full((17,2,4),.5); expected[...,0]=100; expected[...,1]=-100
        spec=self.spec('tiff','float32')
        target=ref.output_pixels(expected,spec)
        actual=target.astype(np.float32)
        actual[7,0,0]+=np.float32(.001) # larger than absolute, within relative bound
        compare=ref.compare
        for fails in (False,True):
            with self.subTest(fails=fails):
                if fails:
                    actual[8,0,2]+=np.float32(.00006)
                    actual[16,0,1]-=np.float32(.003)
                oracle=compare(actual,target,geometry_changed=True)
                maximum=float(np.abs(actual.astype(np.float64)-target.astype(np.float64)).max())
                rows=[]
                def block_compare(a,b,**kwargs):
                    self.assertLessEqual(a.shape[0],8)
                    rows.append(a.shape[0])
                    return compare(a,b,**kwargs)
                with patch.object(ref,'compare',side_effect=block_compare):
                    result=rb.compare_output_rows(actual,expected,spec)
                self.assertEqual(result['pixel_pass'],not fails)
                self.assertEqual(result['pixel_pass'],oracle['pass_'])
                self.assertEqual(result['max_absolute_error'],maximum)
                self.assertEqual(result['tolerance'],{'absolute':ref.GEOMETRY_ABS_TOL,'relative':ref.REL_TOL})
                self.assertEqual(rows,[8,8,1])

    def test_nonfinite_expected_or_actual_is_rejected_in_later_blocks(self):
        np=self.np
        for row,invalid in ((7,float('nan')),(8,float('inf')),(16,-float('inf'))):
            with self.subTest(row=row):
                expected=np.zeros((17,2,4)); expected[row,0,0]=invalid
                with patch.object(rb,'read') as reader:
                    with self.assertRaisesRegex(ValueError,'expected reference shape/nonfinite'):
                        rb.verify('unused',expected,self.spec('tiff','float32'),{})
                    reader.assert_not_called()
                expected[row,0,0]=0
                data=np.zeros((17,2,4),dtype=np.float32); data[0,0,1]=1
                data[row,0,0]=invalid # Earlier tolerance failure must not skip this block.
                with patch.object(rb,'read',return_value=(data,self.info('tiff',32,width=2,height=17))):
                    with self.assertRaisesRegex(ValueError,'nonfinite readback difference'):
                        rb.verify('unused',expected,self.spec('tiff','float32'),{})

    def test_resize_precedes_blockwise_nonlinear_conversion_without_changing_pixels(self):
        np=self.np
        expected=np.empty((17,5,4))
        for y in range(17):
            for x in range(5):expected[y,x]=[.1+x/20,.1+y/40,.2+(x+y)/100,.25 if (x+y)%2 else .75]
        original=expected.copy()
        for edge in (9,31):
            with self.subTest(edge=edge):
                spec=self.spec(depth='sixteen',composite=True)
                spec['size']={'mode':'fit','width':edge,'height':edge,'allow_upscale':True}
                spec['profile']={'kind':'icc','bytes':list(ref.matrix_profile(2))}
                target=ref.output_pixels(expected,spec)
                height,width=target.shape[:2]
                info=self.info(bits=16,width=width,height=height,composite=True)
                info['icc']=ref.matrix_profile(2)
                with patch.object(rb,'read',return_value=(target,info)):
                    result=rb.verify('unused',expected,spec,{})
                self.assertTrue(result['pixel_pass'])
                self.assertEqual(result['max_absolute_error'],0)
                self.assertTrue(np.array_equal(expected,original),'readback mutated its linear reference')

    def test_constant_jpeg_threshold_and_general_lossy_scope_are_unchanged(self):
        np=self.np
        expected=np.full((17,2,4),.5); expected[...,3]=1
        spec=self.spec('jpeg',composite=True); spec['format']['quality']=90
        data=ref.output_pixels(expected,spec)
        data[7,0,0]+=3
        self.assertTrue(rb.compare_output_rows(data,expected,spec,constant_jpeg=True)['pixel_pass'])
        data[8,0,0]+=4
        result=rb.compare_output_rows(data,expected,spec,constant_jpeg=True)
        self.assertFalse(result['pixel_pass']); self.assertEqual(result['max_absolute_error'],4)
        result=rb.compare_output_rows(data,expected,spec)
        self.assertIsNone(result['pixel_pass']); self.assertIsNone(result['tolerance'])
        self.assertEqual(result['max_absolute_error'],4)
        spec['format']['quality']=80
        with self.assertRaisesRegex(ValueError,'quality90'):
            rb.compare_output_rows(data,expected,spec,constant_jpeg=True)

if __name__=='__main__':unittest.main()
