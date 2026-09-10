"""Fixed supplemental correctness requests; no generation or execution on import."""
from __future__ import annotations
import copy
import edit_qualification as q
import edit_reference as ref


def metadata(extended=False):
    payload='qualification '+'x'*80000 if extended else 'qualification Café &amp; 東京'
    xmp=('<?xpacket begin="\ufeff" id="W5M0MpCehiHzreSzNTczkc9d"?>'
         '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">'
         '<rdf:Description rdf:about="urn:photocatalog:qualification:subject" xmlns:q="https://photocatalog.invalid/qualification/1/" '
         'xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:tiff="http://ns.adobe.com/tiff/1.0/" '
         'xmlns:exif="http://ns.adobe.com/exif/1.0/" xmlns:aux="http://ns.adobe.com/exif/1.0/aux/" '
         'xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"><q:payload>'+payload+'</q:payload>'
         '<q:qualified rdf:parseType="Resource"><rdf:value>42</rdf:value><q:unit>mm</q:unit></q:qualified>'
         '<q:structure rdf:parseType="Resource"><q:child q:flag="yes">value</q:child></q:structure>'
         '<q:bag><rdf:Bag><rdf:li>A</rdf:li><rdf:li>B</rdf:li></rdf:Bag></q:bag>'
         '<q:ordered><rdf:Seq><rdf:li>first</rdf:li><rdf:li>second</rdf:li></rdf:Seq></q:ordered>'
         '<crs:Exposure2012>2.0</crs:Exposure2012><crs:HasSettings>True</crs:HasSettings>'
         '<tiff:Orientation>8</tiff:Orientation><tiff:ImageWidth>999</tiff:ImageWidth>'
         '<tiff:ImageLength>777</tiff:ImageLength><tiff:SampleFormat>3</tiff:SampleFormat>'
         '<tiff:StripOffsets>12345</tiff:StripOffsets><exif:MakerNote>stale pointer</exif:MakerNote>'
         '<exif:UserComment>preserve this user text</exif:UserComment>'
         '<tiff:Make>Analytic Camera</tiff:Make><tiff:Model>Matrix RGB</tiff:Model>'
         '<aux:Lens>Fixed 50 mm</aux:Lens><tiff:Artist>Qualification</tiff:Artist>'
         '<tiff:Copyright>CC0</tiff:Copyright><tiff:ImageDescription>Rendered fixture</tiff:ImageDescription>'
         '<exif:DateTimeOriginal>2026-01-02T03:04:05</exif:DateTimeOriginal>'
         '<exif:ExposureTime>1/125</exif:ExposureTime><exif:FNumber>28/10</exif:FNumber>'
         '<exif:FocalLength>50/1</exif:FocalLength><exif:ISOSpeedRatings>100</exif:ISOSpeedRatings>'
         '<dc:title><rdf:Alt><rdf:li xml:lang="x-default">Photo</rdf:li>'
         '<rdf:li xml:lang="fr">Photographie</rdf:li></rdf:Alt></dc:title>'
         '</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>')
    return dict(xmp=xmp,exif=dict(make='Analytic Camera',model='Matrix RGB',lens='Fixed 50 mm',
        date_time_original='2026:01:02 03:04:05',artist='Qualification',copyright='CC0',
        description='Rendered fixture',exposure_time=dict(numerator=1,denominator=125),
        f_number=dict(numerator=28,denominator=10),iso=100,focal_length=dict(numerator=50,denominator=1)))


def output_matrix():
    profiles=[dict(kind='srgb'),dict(kind='linear_srgb'),dict(kind='icc',bytes=list(ref.matrix_profile(1))),
              dict(kind='icc',bytes=list(ref.matrix_profile(2)))]
    sizes=[dict(mode='original'),dict(mode='fit',width=12,height=12,allow_upscale=False),
           dict(mode='fit',width=96,height=96,allow_upscale=True),
           dict(mode='fit',width=96,height=96,allow_upscale=False)]
    cases=[]
    for name,base in q.outputs().items():
        selected=[dict(kind='linear_srgb'),profiles[2]] if name=='tiff32' else profiles
        alphas=[dict(mode='composite',linear_rgb=[.125,.25,.5])]
        if name!='jpeg8':
            alphas.append(dict(mode='preserve'))
        for pi,profile in enumerate(selected):
            for si,size in enumerate(sizes):
                for ai,alpha in enumerate(alphas):
                    output=copy.deepcopy(base)
                    output.update(profile=profile,size=size,alpha=alpha)
                    cases.append(dict(id=f'format-{name}-profile{pi}-size{si}-alpha{ai}',
                        phase='correctness',fixture_id='analytic-flat',operation='neutral',
                        recipes=[q.recipe()],outputs=[output],metadata=metadata(extended=name=='jpeg8'),
                        warmups=0,repetitions=1,deadline_seconds=120,encoded_extent=1024*1024))
    return cases


def analytic_matrix():
    cases=[]
    for fixture in ('analytic-signed-alpha','analytic-impulse','analytic-noise','analytic-flat'):
        for repeat in (0,1):
            cases.append(dict(id=f'analytic-{fixture}-{repeat}',phase='correctness',fixture_id=fixture,
                operation='analytic-all',recipes=[v[0] for v in q.recipes().values()],outputs=[],
                metadata={},warmups=0,repetitions=1,deadline_seconds=120))
    return cases


def support_matrix():
    # Capability checks only. No 32MP timing budget is applied to 100MP.
    limits=dict(decode=dict(max_encoded_bytes=2*q.GIB,max_intermediate_pixels=110_000_000,
                           max_allocation_bytes=4*q.GIB),
                render=dict(max_pixels=100_000_000,max_allocation_bytes=4*q.GIB,max_live_bytes=12*q.GIB),
                encoded_extent=2*q.GIB,sampled_worker_rss_stop_bytes=12*q.GIB,
                sampled_group_rss_stop_bytes=12*q.GIB+512*q.MIB)
    cases=[dict(id='support-100mp-admitted',phase='support100mp',fixture_id='support-100mp',
               operation='combined',recipes=[q.recipe(),q.recipes()['combined'][0]],
               outputs=list(q.outputs().values()),warmups=0,repetitions=1,
               limits=limits,deadline_seconds=3600)]
    refused=copy.deepcopy(limits)
    refused['decode']['max_allocation_bytes']=1024
    cases.append(dict(id='support-100mp-refused',phase='refusal',fixture_id='support-100mp',
        operation='decode_allocation',recipes=[q.recipe()],outputs=[],warmups=0,repetitions=1,
        limits=refused,deadline_seconds=120))
    return cases


def failures():
    cases=[]
    for operation in ('decode_allocation','render_allocation','encoded_extent','canceled','source_changed','invalid_profile'):
        output=q.outputs()['tiff32'] if operation=='invalid_profile' else q.outputs()['png16']
        if operation=='invalid_profile':
            output['profile']={'kind':'srgb'}
        cases.append(dict(id='refusal-'+operation,phase='refusal',fixture_id='analytic-flat',
            operation=operation,recipes=[q.recipe()],outputs=[output],warmups=0,repetitions=1,
            deadline_seconds=120))
    return cases


def proxy_references():
    pairs=[value for key,pair in q.recipes().items() if key!='neutral' for value in pair]
    return [dict(id='proxy-reference-'+fixture,phase='proxy_reference',fixture_id=fixture,
                 operation='all-pairs',recipes=copy.deepcopy(pairs),outputs=[],warmups=0,repetitions=1,
                 deadline_seconds=1200) for fixture in q.TIMING]


def durable_metadata_cases():
    return [dict(id='durable-metadata-'+name,phase='export_correctness',fixture_id='analytic-metadata',
                 operation=name,recipes=[q.recipe()],outputs=[copy.deepcopy(output)],
                 resolve_embedded=True,warmups=0,repetitions=1,deadline_seconds=180)
            for name,output in q.outputs().items()]


def overlap_cases():
    return [dict(id='foreground-'+phase,phase=phase,fixture_id='private-X-T3-RAW',
                 operation='exposure',recipes=copy.deepcopy(q.recipes()['exposure']),
                 outputs=[copy.deepcopy(q.outputs()['tiff16'])] if phase=='overlap_export' else [],
                 warmups=0,repetitions=100,deadline_seconds=300)
            for phase in ('overlap_import','overlap_export')]
