"""Fixed S8 timing distributions; these do not establish pixel/ownership acceptance.

Inputs must additionally pass independent case and process-lifetime verification.
Every distribution stays within one fixture/request/recipe configuration. Warmups
remain in the sample ledger but cannot contribute to timing awards.
"""
import math

COUNTS = {
    'kernel': (2, 100), 'warm_service': (2, 100),
    'full': (2, 20), 'first_raw': (2, 20), 'export': (2, 20),
    'overlap_import': (0, 100), 'overlap_export': (0, 100),
    'correctness': (0, 1), 'support100mp': (0, 1), 'refusal': (0, 1),
    'proxy_reference': (0, 1), 'export_correctness': (0, 1),
    'large_cancellation': (0, 1),
}
PIXEL_PHASES = {'correctness', 'kernel', 'full', 'support100mp', 'proxy_reference', 'large_cancellation'}
TARGET_MS = {'kernel': 100, 'warm_service': 250, 'full': 5000,
             'first_raw': 20000, 'overlap_import': 100, 'overlap_export': 100}
EXPORT_MS = {'jpeg': 25000, 'png': 40000, 'tiff': 30000}
RENDER_FIELDS = ('source_verification_before_ms', 'staging_setup_ms', 'decode_ms',
                 'recipe_ms', 'metadata_ms', 'encode_ms',
                 'source_verification_after_ms', 'sync_ms', 'total_ms')


def nonnegative(value):
    if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
        raise ValueError('finite nonnegative measured milliseconds required')
    return float(value)


def distribution(values):
    if not isinstance(values, list) or not values or len(values) > 3264:
        raise ValueError('bounded nonempty timing sample list required')
    ordered = sorted(nonnegative(value) for value in values)
    def rank(percent):
        return ordered[(len(ordered)*percent + 99)//100 - 1]
    return dict(count=len(ordered), min_ms=ordered[0], p50_ms=rank(50),
                p95_ms=rank(95), p99_ms=rank(99), max_ms=ordered[-1])


def expected_identities(request):
    phase = request['phase']
    if phase not in COUNTS:
        raise ValueError('unknown fixed phase')
    warmups, repetitions = request['warmups'], request['repetitions']
    if type(warmups) is not int or type(repetitions) is not int or (warmups, repetitions) != COUNTS[phase]:
        raise ValueError('fixed sample counts changed')
    recipes = request['recipes']
    if not isinstance(recipes, list) or not 1 <= len(recipes) <= 32:
        raise ValueError('bounded recipe configuration required')
    if phase in ('warm_service', 'first_raw') and (len(recipes) != 2 or recipes[0] == recipes[1]):
        raise ValueError('two distinct frozen delivery bases required')
    if phase in ('export', 'export_correctness') and (len(recipes) != 1 or len(request['outputs']) != 1):
        raise ValueError('single export configuration required')
    recipe_count = len(recipes) if phase in PIXEL_PHASES else 1
    return {(recipe, iteration) for recipe in range(recipe_count)
            for iteration in range(warmups + repetitions)}


def sample_identity(sample, request):
    recipe = sample.get('recipe_index', 0)
    iteration = sample['iteration']
    if type(recipe) is not int or type(iteration) is not int:
        raise ValueError('integer recipe/iteration identity required')
    if request['phase'] in PIXEL_PHASES and 'recipe_index' not in sample:
        raise ValueError('pixel sample lacks explicit recipe identity')
    return recipe, iteration


def export_timings(value):
    phases = value['phases']
    result = {name: nonnegative(phases[name]) for name in
              ('worker_elapsed_ms', 'seal_ms', 'accept_ms', 'publication_elapsed_ms')}
    result.update({'render.'+name: nonnegative(phases['render'][name]) for name in RENDER_FIELDS})
    publication = phases['publication']
    result.update({'publication.'+name: nonnegative(publication[name])
                   for name in ('original_hash_ms', 'total_ms')})
    result.update({'publication.filesystem.'+name: nonnegative(publication['publication'][name])
                   for name in ('hash_ms', 'capture_ms', 'link_ms', 'durability_ms')})
    intervals = publication['authority_intervals_ms']
    # Each qualification export starts a fresh job: intent, capture, link, finish.
    # Recovery has different paths and cannot silently enter this distribution.
    if not isinstance(intervals, list) or len(intervals) != 4:
        raise ValueError('fresh export requires four actual authority intervals')
    result.update({'publication.authority_'+str(index)+'_ms': nonnegative(item)
                   for index, item in enumerate(intervals)})
    return result


def summarize_case(request, observations):
    expected = expected_identities(request)
    if not isinstance(observations, list) or len(observations) != len(expected):
        raise ValueError('missing or extra observations')
    phase = request['phase']
    seen = set()
    groups = {}
    for value in observations:
        if value.get('kind') != 'observation':
            raise ValueError('non-observation in timing ledger')
        identity = sample_identity(value, request)
        if identity not in expected or identity in seen:
            raise ValueError('duplicate or out-of-range sample identity')
        seen.add(identity)
        is_warmup = identity[1] < request['warmups']
        if type(value.get('warmup')) is not bool or value['warmup'] != is_warmup:
            raise ValueError('warmup classification differs')
        if phase == 'refusal':
            continue  # A typed refusal has no successful-operation latency award.
        elapsed = nonnegative(value['elapsed_ms'])
        details = export_timings(value) if phase in ('export', 'export_correctness') else {}
        if not is_warmup:
            group = groups.setdefault(identity[0], {'elapsed_ms': [], 'phases': {}})
            group['elapsed_ms'].append(elapsed)
            for name, duration in details.items():
                group['phases'].setdefault(name, []).append(duration)
    if seen != expected:
        raise ValueError('incomplete identity set')
    target = TARGET_MS.get(phase)
    if phase == 'export':
        target = EXPORT_MS[request['outputs'][0]['format']['format']]
    result = []
    for recipe_index, group in sorted(groups.items()):
        elapsed = distribution(group['elapsed_ms'])
        if elapsed['count'] != request['repetitions']:
            raise ValueError('wrong measured sample count')
        result.append(dict(recipe_index=recipe_index, elapsed=elapsed,
                           phases={name: distribution(values) for name, values in sorted(group['phases'].items())},
                           p95_target_ms=target,
                           numeric_target_met=None if target is None else elapsed['p95_ms'] <= target))
    return dict(version=1, fixture_id=request['fixture_id'], operation=request['operation'],
                phase=phase, source_sha256=request['source_sha256'],
                width=request['width'], height=request['height'],
                recipes=request['recipes'], outputs=request['outputs'],
                warmups_per_configuration=request['warmups'], sample_count=len(observations),
                percentile_method='nearest_rank', configurations=result,
                independent_case_and_process_verification_required=True,
                whole_story_qualified=False)
