"""Bounded evidence contracts for supplementary Lightroom full/path/packet control.

No SQLite access, source enumeration, native execution, or automatic selection.
Byte budgets are prospective, evidence-derived admission estimates, not quotas.
"""
import hashlib
import json
import os
from pathlib import Path
import stat

MIB = 1024**2
CAP = 16*MIB
RESERVE = 32*1024*MIB
PHASES = {'full', 'paths', 'packets'}
CATEGORIES = {
    'full': {'capture_raw', 'capture_working', 'capture_logical', 'final_plan', 'all_row_pages',
             'reports_and_commands', 'sqlite_temporary', 'control_evidence'},
    'paths': {'path_plan_growth', 'path_pages', 'reports_and_commands', 'sqlite_temporary', 'control_evidence'},
    'packets': {'retained_packets', 'decoded_packets', 'metadata_projection', 'packet_and_path_pages',
                'reports_and_commands', 'sqlite_temporary', 'control_evidence'},
}


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=True, allow_nan=False).encode()+b'\n'


def sha(data):
    return hashlib.sha256(data).hexdigest()


def absolute(value):
    p = Path(value)
    if not p.is_absolute() or '..' in p.parts or any(x.is_symlink() for x in [p, *p.parents]):
        raise ValueError('absolute non-symlink evidence/control path required')
    return p


def raw(path, cap=CAP):
    path = absolute(path)
    fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size > cap:
            raise ValueError('nonregular/oversized evidence')
        data = stream.read(cap+1)
        after = os.fstat(stream.fileno())
    if len(data) > cap or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns):
        raise ValueError('evidence changed while reading')
    return data


def reference(path):
    return {'path': str(absolute(path)), 'sha256': sha(raw(path))}


def checked(ref, cap=CAP):
    if not isinstance(ref, dict) or set(ref) != {'path', 'sha256'} or not isinstance(ref['sha256'], str) or len(ref['sha256']) != 64:
        raise ValueError('unresolved evidence reference')
    data = raw(ref['path'], cap)
    if sha(data) != ref['sha256']:
        raise ValueError('evidence digest differs')
    return data


def document(ref):
    return json.loads(checked(ref))


def integer(value, minimum=0):
    if type(value) is not int or value < minimum:
        raise ValueError('invalid admission integer')
    return value


def unchanged(value):
    return (value.get('before_complete') is True and value.get('after_complete') is True
            and all(value.get(k) == [] for k in ['added', 'removed', 'changed']))


def no_award(value):
    if value.get('automatic_selection') is not False or value.get('migration_executed') is not False:
        raise ValueError('unexpected selection/migration claim')


def pointer(value, path):
    if not isinstance(path, list) or len(path) > 32:
        raise ValueError('bounded literal JSON pointer required')
    for key in path:
        if isinstance(value, list):
            value = value[integer(key)]
        elif isinstance(value, dict) and isinstance(key, str):
            value = value[key]
        else:
            raise ValueError('invalid evidence pointer')
    return value


def amount(term, load=document):
    """Exact arithmetic over pinned scalar observations; multipliers are disclosed.

    No eval, DB scans or arbitrary Python expressions. A reviewer supplies growth
    factors/headroom with rationale; observed byte totals alone are not maxima.
    """
    if set(term) != {'basis', 'numerator', 'denominator', 'reason'} or not term['reason'].strip():
        raise ValueError('funding rationale and exact formula required')
    if not 1 <= len(term['basis']) <= 16384:
        raise ValueError('funding observation bound')
    total = 0
    for item in term['basis']:
        if set(item) != {'reference', 'pointer'}:
            raise ValueError('funding basis must name immutable scalar evidence')
        total += integer(pointer(load(item['reference']), item['pointer']))
    numerator = integer(term['numerator'], 1)
    denominator = integer(term['denominator'], 1)
    return (total*numerator+denominator-1)//denominator


def funding(ref, phase, input_sha):
    proof = document(ref)
    if (set(proof) != {'protocol', 'phase', 'input_sha256', 'categories', 'single_command_headroom', 'protected_bytes', 'reserve_bytes'}
            or proof['protocol'] != 1 or proof['phase'] != phase or proof['input_sha256'] != input_sha
            or set(proof['categories']) != CATEGORIES[phase] or proof['reserve_bytes'] != RESERVE):
        raise ValueError('incomplete/wrong phase funding derivation')
    used = 0
    def load(ref):
        nonlocal used
        data = checked(ref)
        used += len(data)
        if used > 128*MIB:
            raise ValueError('cumulative funding metadata read admission exceeded')
        return json.loads(data)
    values = {name: amount(term, load) for name, term in proof['categories'].items()}
    headroom = amount(proof['single_command_headroom'], load)
    integer(headroom, 1)
    protected = integer(proof['protected_bytes'])
    total = sum(values.values())
    integer(total, 1)
    return {'categories': values, 'phase_remaining_bytes': total, 'command_buffer_bytes': headroom,
            'protected_bytes': protected, 'operating_reserve_bytes': RESERVE,
            'initial_minimum_bytes': protected+RESERVE+total+headroom,
            'ongoing_pause_bytes': protected+RESERVE+headroom,
            'ongoing_emergency_bytes': protected+RESERVE,
            'meaning': 'prospective evidence-derived growth estimate plus separate command headroom; sampled guards, not a quota'}


def context(recipe):
    phase = recipe['phase']; run = absolute(recipe['run']); value = document(recipe['input'])
    if phase not in PHASES:
        raise ValueError('unsupported phase')
    if phase == 'full':
        if recipe['paths_review'] != {'kind': 'not_applicable'}:
            raise ValueError('unexpected path prerequisite on full')
        main = document(recipe['baseline'])
        no_award(main)
        if recipe['baseline']['path'] != str(run/'reports/main-review.json') or value.get('main_review_sha256') != recipe['baseline']['sha256']:
            raise ValueError('full request does not bind this main review')
        if main.get('inventory_complete') is not True or not unchanged(main.get('inventory_delta', {})) or value.get('automatic_selection') is not False:
            raise ValueError('changed/incomplete main inventory or automatic choice')
        outcomes = {v['key']: v for v in main['outcomes']}
        if len(outcomes) != main['candidate_count'] or len(outcomes) != len(main['outcomes']):
            raise ValueError('main candidate counts differ')
        families = {m['revision_id']: f for f in main['families']['families'] for m in f['members']}
        requested = {}
        for item in value['requests']:
            key = item['candidate_key']; prior = outcomes[key]; rev = prior['inspection']['revision']; family = families[rev]
            if (key in requested or item['revision'] != rev or item['family_evidence_digest'] != family['evidence_digest']
                    or not item['reason'].strip() or item['role'] not in {'prospective_suggestion', 'additional_ambiguity_evidence'}
                    or (item['role'] == 'prospective_suggestion' and family['suggested'] != rev)):
                raise ValueError('invalid full member/evidence/role')
            requested[key] = prior
        if not requested:
            raise ValueError('empty full request')
        digest = sha(encoded(value)); plan = run/'plans'/('final-'+digest)
        sources = []
        for prior in requested.values():
            native = prior['candidate']['path']
            if native['encoding'] != 'UnixBytes' or b'\0' in bytes(native['units']):
                raise ValueError('foreign capture source needs explicit mapping')
            sources.append(os.fsdecode(bytes(native['units'])))
        return {'phase': phase, 'tag': ['full', digest], 'id': digest, 'plan': str(plan),
                'output': str(run/'reports'/('full-review-'+digest+'.json')), 'requested': requested,
                'all_outcomes': outcomes, 'sources': sources, 'input': value}
    if recipe['baseline'] != recipe['input']:
        raise ValueError('paths/packets baseline must be the exact full review')
    no_award(value)
    if not unchanged(value.get('inventory_delta', {})):
        raise ValueError('changed/incomplete full inventory')
    digest = value['request_id']
    if (digest != sha(encoded(value['request'])) or value['plan'] != str(run/'plans'/('final-'+digest))
            or recipe['input']['path'] != str(run/'reports'/('full-review-'+digest+'.json'))):
        raise ValueError('full review path/request/plan provenance differs')
    main_ref = reference(run/'reports/main-review.json')
    if main_ref['sha256'] != value['request']['main_review_sha256']:
        raise ValueError('full review imported main baseline differs')
    main = document(main_ref)
    priors = {v['key']: v for v in main['outcomes']}
    request_keys = [v['candidate_key'] for v in value['request']['requests']]
    if len(set(request_keys)) != len(request_keys) or not set(request_keys) <= set(priors):
        raise ValueError('full request candidate roster differs')
    validate_output(value, {'phase': 'full', 'input': value['request'], 'id': digest, 'plan': value['plan'],
                           'all_outcomes': priors, 'requested': {k: priors[k] for k in request_keys}})
    requested = {}
    for item in value['outcomes']:
        if item['full_requested']:
            full = item.get('full') or {}; capture = full.get('capture') or {}; inspection = item.get('inspection') or {}
            if (full.get('ok') is not True or capture.get('state') != 'captured'
                    or capture.get('raw_byte_retention') != 'complete' or capture.get('sqlite_consistency') != 'consistent_default_sqlite'
                    or inspection.get('capture_path') != full.get('capture_path') or not inspection.get('revision') or 'error' in item):
                raise ValueError('full capture failed or main-only fallback')
            requested[item['key']] = item
    if not requested or len(requested) != len(value['request']['requests']):
        raise ValueError('full requested member count differs')
    if phase == 'packets':
        prior = document(recipe['paths_review'])
        if recipe['paths_review']['path'] != str(run/'reports'/('paths-review-'+recipe['input']['sha256']+'.json')):
            raise ValueError('wrong metadata-only path review')
        validate_path_result(prior, requested, recipe['input']['sha256'])
    elif recipe['paths_review'] != {'kind': 'not_applicable'}:
        raise ValueError('unexpected packet prerequisite')
    key = recipe['input']['sha256']
    return {'phase': phase, 'tag': [phase, key], 'id': key, 'plan': value['plan'],
            'output': str(run/'reports'/(phase+'-review-'+key+'.json')), 'requested': requested, 'input': value}


def validate_path_result(value, requested, full_sha):
    no_award(value)
    if value.get('full_review_sha256') != full_sha or not unchanged(value.get('inventory_delta', {})):
        raise ValueError('path/packet provenance/inventory differs')
    expected = {k: v['inspection']['revision'] for k, v in requested.items()}
    actual = {}
    for row in value['outcomes']:
        if row['key'] in actual or 'error' in row or row['report']['revision_id'] != row['revision']:
            raise ValueError('failed/duplicate/wrong path result')
        if set(row['pages']) != {'paths', 'packets', 'metadata-conflicts', 'issues'}:
            raise ValueError('missing evidence pages')
        actual[row['key']] = row['revision']
    if actual != expected or value['outcome_counts'] != {'requested_members': len(expected), 'failures': 0}:
        raise ValueError('path/packet outcome roster differs')


def validate_output(value, ctx):
    no_award(value)
    if ctx['phase'] != 'full':
        validate_path_result(value, ctx['requested'], ctx['id']); return
    if (value.get('request') != ctx['input'] or value.get('request_id') != ctx['id'] or value.get('plan') != ctx['plan']
            or not unchanged(value.get('inventory_delta', {}))):
        raise ValueError('full output provenance/inventory differs')
    outcomes = {v['key']: v for v in value['outcomes']}
    if len(outcomes) != len(value['outcomes']) or set(outcomes) != set(ctx['all_outcomes']):
        raise ValueError('full output candidate roster differs')
    requested = set(ctx['requested'])
    for key, row in outcomes.items():
        prior = ctx['all_outcomes'][key]
        if (row['full_requested'] != (key in requested) or 'error' in row or not row.get('inspection')
                or row.get('source') != prior['candidate']['path']):
            raise ValueError('failed/missing full inspection')
        if key in requested:
            full = row.get('full') or {}; capture = full.get('capture') or {}
            if (full.get('ok') is not True or capture.get('state') != 'captured' or capture.get('raw_byte_retention') != 'complete'
                    or capture.get('sqlite_consistency') != 'consistent_default_sqlite'
                    or row['inspection']['capture_path'] != full.get('capture_path')
                    or row['inspection']['revision'] != capture.get('revision_id')):
                raise ValueError('failed full preservation is not a successful review')
        elif (row.get('full') is not None or row['inspection'].get('capture_path') != prior.get('capture_path')
              or row['inspection'].get('revision') != (prior.get('capture') or {}).get('revision_id')):
            raise ValueError('main-only fallback capture/revision differs')
    counts = value['outcome_counts']
    if counts != {'candidates': len(outcomes), 'full_requested': len(requested), 'full_capture_failures': 0,
                  'inspection_failures': 0, 'main_only_members': len(outcomes)-len(requested)}:
        raise ValueError('full counts differ')
