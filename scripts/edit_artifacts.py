"""Cross-case artifacts for exact current previews and durable export readbacks."""
import re
import struct
import edit_reference as reference
import edit_readback as readback
import edit_derivative
import edit_correctness_matrix as matrix


def reference_case(root,request,case_id,recipe):
    from edit_verify import read_json,observations,sample_coverage,digest,owned,MAX_JSON,MAX_SAMPLES
    if not re.fullmatch('[A-Za-z0-9_-]{1,120}',case_id):raise ValueError('invalid reference case')
    directory=owned(root.parent,root.parent/(case_id+'-output'))
    other=read_json(directory/'request.json')
    receipt=read_json(directory/'receipt.json')
    for field in ('fixture_id','source_blake3','source_sha256','width','height'):
        if other[field]!=request[field]:raise ValueError('reference source differs')
    if not receipt.get('probe_complete'):raise ValueError('reference probe incomplete')
    attempts,values=observations(directory)
    sample_coverage(other,attempts,values)
    try:index=other['recipes'].index(recipe)
    except ValueError as exc:raise ValueError('reference lacks exact requested recipe') from exc
    selected=[v for v in values if v['recipe_index']==index and v['iteration']==0]
    if len(selected)!=1:raise ValueError('reference sample is not unique')
    proof=dict(id=case_id,request_sha256=digest(directory/'request.json','sha256',MAX_JSON),
               receipt_sha256=digest(directory/'receipt.json','sha256',MAX_JSON),
               samples_sha256=digest(directory/'samples.jsonl','sha256',MAX_SAMPLES))
    return directory,selected[0],proof


def linear_reference(directory,value):
    from edit_verify import digest,owned
    pixels=value['pixels']
    path=owned(directory,pixels['raw'])
    size=pixels['width']*pixels['height']*16
    if path.stat().st_size!=size or digest(path,'blake3',size)!=pixels['rgba_f32le_blake3']:
        raise ValueError('reference linear bytes differ')
    np=reference.np_module()
    return np.memmap(path,mode='r',dtype='<f4',shape=(pixels['height'],pixels['width'],4))


def service_artifact(root,request,value):
    from edit_verify import digest,owned
    from blake3 import blake3
    recipe=request['recipes'][value['iteration']%len(request['recipes'])]
    directory,expected,proof=reference_case(root,request,'proxy-reference-'+request['fixture_id'],recipe)
    artifact=expected['preview_reference']
    path=owned(directory,artifact['path'])
    if digest(path,'blake3',8*1024*1024)!=artifact['blake3'] or value['encoded_blake3']!=artifact['blake3']:
        raise ValueError('current preview differs from exact linear recipe reference')
    pixels,info=readback.read(path,max_pixels=1600*1600,max_encoded_bytes=8*1024*1024,
                              max_decoded_bytes=1600*1600*4)
    if info['format']!='jpeg':raise ValueError('current preview format differs')
    basis=value['iteration']%len(request['recipes'])
    rgb_path=owned(root,root/f'delivery-basis-{basis}.rgb')
    height,width=pixels.shape[:2]
    if rgb_path.stat().st_size!=width*height*3:raise ValueError('delivered RGB shape differs')
    with rgb_path.open('rb') as stream:raw=stream.read(width*height*3+1)
    if len(raw)!=width*height*3:raise ValueError('delivered RGB artifact changed')
    framed=blake3(struct.pack('<II',width,height));framed.update(raw)
    if framed.hexdigest()!=value['decoded_blake3']:
        raise ValueError('delivered current RGB differs from retained actual decoder bytes')
    np=reference.np_module()
    actual=np.frombuffer(raw,dtype=np.uint8).reshape(height,width,3)
    # Exact encoded-reference equality and actual delivered-byte identity are
    # gates. Independent JPEG IDCT/color rounding is reported separately; no
    # cross-decoder bit-exact promise is added to the approved codec contract.
    independent_max_abs=int(np.abs(actual.astype(np.int16)-pixels.astype(np.int16)).max())
    retained=value.get('retained_encoded')
    if value['iteration']<len(request['recipes']):
        if not retained or digest(owned(root,retained),'blake3',8*1024*1024)!=artifact['blake3']:
            raise ValueError('required complete delivery artifact missing')
    elif retained is not None:raise ValueError('unexpected extra retained delivery')
    key=value['key']
    if (key['fingerprint']!=request['source_blake3'] or key['edge']!=1600
        or key['edit_revision']!=value['iteration']+1
        or not key['renderer_version'].endswith(':proxy1600')):
        raise ValueError('delivery key/revision/source differs')
    if not value['observed_worker_pids'] or not value['native_drained']:
        raise ValueError('delivery did not complete a new owned worker lifecycle')
    source=value['record']['edit_input']
    if request['phase']=='first_raw':
        if source['source']!='original_decoded':raise ValueError('first RAW reused prepared pixels')
    elif not value['warmup']:
        if source['source']!='prepared_proxy':raise ValueError('warm delivery lacks prepared identity')
        identity=source['receipt']['identity']
        if (identity['source_fingerprint']!=request['source_blake3'] or identity['white_balance']!=recipe['settings']['white_balance']
            or identity['longest_edge']!=1600 or identity['original_dimensions']!=[request['width'],request['height']]):
            raise ValueError('prepared input does not match selected WB/source/geometry')
    proof=dict(proof,independent_jpeg_decoder_max_abs=independent_max_abs)
    return proof


def export_artifact(root,request,value):
    from edit_verify import digest,owned
    path=owned(root,value['path'])
    if digest(path,'blake3',request['encoded_extent'])!=value['blake3']:
        raise ValueError('published output bytes differ')
    if value['job']['state']!='complete' or value['job']['completed']!=1 or value['job']['total']!=1:
        raise ValueError('export job not durably completed')
    if len(value['items'])!=1 or value['items'][0]['state']!='published':
        raise ValueError('export item not durably published')
    recipe=request['recipes'][0]
    if request['phase']=='export_correctness':
        import edit_fixtures
        linear=reference.render(reference.fixture_to_linear(edit_fixtures.pixels('analytic-metadata')),recipe)
        metadata=edit_derivative.expected_metadata(matrix.metadata(True),linear.shape[1],linear.shape[0],request['outputs'][0])
        dependencies=[]
    else:
        directory,expected,proof=reference_case(root,request,'correctness-'+request['fixture_id']+'-combined',recipe)
        linear=linear_reference(directory,expected)
        metadata=edit_derivative.expected_metadata({},linear.shape[1],linear.shape[0],request['outputs'][0])
        dependencies=[proof]
    result=readback.verify(path,linear,request['outputs'][0],metadata,
                           constant_jpeg=request['phase']=='export_correctness',
                           max_encoded_bytes=request['encoded_extent'],
                           max_decoded_bytes=request['render']['max_allocation_bytes'])
    if result['pixel_pass'] is False:raise ValueError('published pixels/metadata differ')
    return result,dependencies
