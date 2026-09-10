"""Deterministic expansion from frozen cases and verified prepared-source roster."""
import copy
from pathlib import Path

EXIF_FIELDS=('make','model','lens','date_time_original','artist','copyright','description',
             'exposure_time','f_number','iso','focal_length')


def normalized_metadata(value):
    if set(value)-{'xmp','exif'} or set(value.get('exif',{}))-set(EXIF_FIELDS):
        raise ValueError('unknown resolved metadata field')
    return dict(xmp=value.get('xmp'),exif={name:value.get('exif',{}).get(name) for name in EXIF_FIELDS})


def expand_request(case,source,normal_limits,root,worker,background_source=None):
    if source['id']!=case['fixture_id']:
        raise ValueError('case source association differs')
    override=case.get('limits',{})
    output=Path(root)/(case['id']+'-output')
    if not output.is_absolute() or not Path(worker).is_absolute() or not Path(source['path']).is_absolute():
        raise ValueError('absolute private request paths required')
    if case['phase']=='overlap_import' and background_source is None:
        raise ValueError('separate verified background copy required')
    return dict(version=1,phase=case['phase'],fixture_id=case['fixture_id'],
        source=source['path'],source_blake3=source['blake3'],source_sha256=source['sha256'],
        width=source['width'],height=source['height'],operation=case['operation'],
        recipes=copy.deepcopy(case['recipes']),outputs=copy.deepcopy(case['outputs']),
        worker=str(worker),output=str(output),
        decode=copy.deepcopy(override.get('decode',normal_limits['decode'])),
        render=copy.deepcopy(override.get('render',normal_limits['render'])),
        encoded_extent=case.get('encoded_extent',override.get('encoded_extent',normal_limits['encoded_extent'])),
        metadata=normalized_metadata(case.get('metadata',{})),
        resolve_embedded=case.get('resolve_embedded',False),
        background_source=str(background_source) if case['phase']=='overlap_import' else None,
        warmups=case['warmups'],repetitions=case['repetitions'])
