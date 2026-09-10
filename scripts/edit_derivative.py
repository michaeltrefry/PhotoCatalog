"""Expected metadata for the declared controlled derivative fixture.

Independent XML construction, no XMP SDK/product helper call. The input is the
explicitly selected whole packet; unknown RDF and subject identities remain.
"""
import copy
import xml.etree.ElementTree as ET
import edit_reference as reference

RDF='http://www.w3.org/1999/02/22-rdf-syntax-ns#'
TIFF='http://ns.adobe.com/tiff/1.0/'
EXIF='http://ns.adobe.com/exif/1.0/'
XMP='http://ns.adobe.com/xap/1.0/'
DC='http://purl.org/dc/elements/1.1/'
PHOTOSHOP='http://ns.adobe.com/photoshop/1.0/'
CRS='http://ns.adobe.com/camera-raw-settings/1.0/'
REMOVED={
    TIFF:set('ImageWidth ImageLength Orientation BitsPerSample SamplesPerPixel PhotometricInterpretation Compression PlanarConfiguration SampleFormat ExtraSamples StripOffsets StripByteCounts RowsPerStrip TileOffsets TileByteCounts TileWidth TileLength JPEGInterchangeFormat JPEGInterchangeFormatLength'.split()),
    EXIF:set('PixelXDimension PixelYDimension ColorSpace ComponentsConfiguration CompressedBitsPerPixel MakerNote'.split()),
    XMP:{'Thumbnails','CreatorTool'},DC:{'format'},PHOTOSHOP:{'ICCProfile'},
    'http://ns.adobe.com/xmp/note/':{'HasExtendedXMP'},
}


def expected_metadata(selected,width,height,spec):
    width,height=reference.output_dimensions(width,height,spec['size'])
    result=copy.deepcopy(selected)
    if result.get('xmp') is None:
        root=ET.Element('{adobe:ns:meta/}xmpmeta')
        rdf=ET.SubElement(root,'{'+RDF+'}RDF')
        ET.SubElement(rdf,'{'+RDF+'}Description',{'{'+RDF+'}about':''})
    else:
        root=ET.fromstring(result['xmp'])
    descriptions=root.findall('.//{'+RDF+'}RDF/{'+RDF+'}Description')
    if len(descriptions)!=1:
        raise ValueError('controlled derivative requires one explicitly selected subject')
    description=descriptions[0]
    def replaced(tag):
        if not tag.startswith('{'): return False
        namespace,name=tag[1:].split('}',1)
        return namespace==CRS or name in REMOVED.get(namespace,set())
    for child in list(description):
        if replaced(child.tag):description.remove(child)
    for name in list(description.attrib):
        if replaced(name):del description.attrib[name]
    fmt=spec['format']
    channels=3 if fmt['format']=='jpeg' or spec['alpha']['mode']=='composite' else 4
    bits={'eight':8,'sixteen':16,'float32':32}[fmt.get('depth','eight')]
    profile=spec['profile']['kind']
    if profile=='icc':
        from blake3 import blake3
        profile_name='ICC BLAKE3 '+blake3(bytes(spec['profile']['bytes'])).hexdigest()
    else:profile_name={'srgb':'sRGB IEC61966-2.1','linear_srgb':'linear sRGB'}[profile]
    values={(TIFF,'ImageWidth'):width,(TIFF,'ImageLength'):height,(TIFF,'Orientation'):1,
            (TIFF,'SamplesPerPixel'):channels,(TIFF,'PhotometricInterpretation'):2,
            (EXIF,'PixelXDimension'):width,(EXIF,'PixelYDimension'):height,
            (EXIF,'ColorSpace'):1 if profile=='srgb' else 65535,
            (PHOTOSHOP,'ICCProfile'):profile_name,(DC,'format'):'image/'+fmt['format'],
            (XMP,'CreatorTool'):'PhotoCatalog'}
    for (namespace,name),value in values.items():
        ET.SubElement(description,'{'+namespace+'}'+name).text=str(value)
    sequence=ET.SubElement(ET.SubElement(description,'{'+TIFF+'}BitsPerSample'),'{'+RDF+'}Seq')
    for _ in range(channels):ET.SubElement(sequence,'{'+RDF+'}li').text=str(bits)
    result['xmp']=ET.tostring(root,encoding='unicode')
    return result
