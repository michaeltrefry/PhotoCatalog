"""Reconcile the complete frozen S8 campaign without running another renderer.

The result qualifies this headless campaign only. Platform delivery and the UI
remain separate story/epic acceptance requirements. Missing evidence is failure.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import edit_disk_budget
import edit_statistics
import edit_verify

MIB = 1024**2
MAX_BINDING = 16*MIB
MAX_REPORT = 64*MIB
PHASE_COVERAGE = {
    'correctness': {'pixel_finite'}, 'kernel': {'pixel_finite'},
    'full': {'pixel_finite'}, 'proxy_reference': {'pixel_finite', 'proxy_artifacts'},
    'support100mp': {'pixel_finite', '100mp_oracle', 'encoded_pixels_metadata'},
    'refusal': {'typed_refusal'}, 'warm_service': {'service_artifacts'},
    'first_raw': {'service_artifacts'}, 'export': {'export_artifacts'},
    'export_correctness': {'export_artifacts', 'encoded_pixels_metadata'},
    'overlap_import': {'live_overlap'}, 'overlap_export': {'live_overlap'},
}


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()


def same(left, right):
    # Rust may normalize integral floats (4300 -> 4300.0). Boolean/integer
    # interchange remains forbidden; it changes the typed request contract.
    if type(left) in (int, float) and type(right) in (int, float):
        return math.isfinite(left) and math.isfinite(right) and left == right
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(same(value, right[key]) for key, value in left.items())
    if isinstance(left, list):
        return len(left) == len(right) and all(same(a, b) for a, b in zip(left, right, strict=True))
    return left == right


def positive_integer(value):
    if type(value) is not int or value <= 0:
        raise ValueError('positive integer evidence value required')
    return value


def bound_path(root, path):
    root = Path(root).resolve(strict=True)
    path = Path(path)
    if not path.is_absolute() or path.is_symlink():
        raise ValueError('absolute ordinary owned evidence path required')
    resolved = path.resolve(strict=True)
    if resolved != path or root not in resolved.parents:
        raise ValueError('indirect or unowned evidence path')
    return resolved


def admitted_file(root, path, limit):
    path = bound_path(root, path)
    if not path.is_file() or path.stat().st_size > limit:
        raise ValueError('owned evidence file admission')
    return path


def supervisor(root, folder, reference, command, limits, expected_pid=None):
    """Validate the retained observation result, launch and all bounded streams."""
    folder = bound_path(root, folder)
    result_path = admitted_file(root, reference, MIB)
    if result_path != folder/'result.json':
        raise ValueError('supervisor result namespace differs')
    result = edit_verify.read_json(result_path, MIB)
    start = edit_verify.read_json(folder/'start.json', MIB)
    if not same(start['command'], command) or not same(start['limits'], limits):
        raise ValueError('supervisor launched a different command or admission')
    ownership = result['ownership']
    if (result.get('complete') is not True or result.get('error') is not None
            or ownership.get('known_absent') is not True
            or ownership.get('root_reaped') is not True
            or type(ownership.get('root_returncode')) is not int
            or ownership['root_returncode'] != 0
            or ownership.get('remaining') != [] or ownership.get('errors') != []):
        raise ValueError('supervisor did not establish successful cleanup')
    spawn = edit_verify.read_json(folder/'spawn.json')
    if expected_pid is not None and positive_integer(spawn['pid']) != positive_integer(expected_pid):
        raise ValueError('probe receipt PID differs from actually launched child')
    count = positive_integer(result['samples'])
    if count > 36002:
        raise ValueError('supervisor sample count exceeds fixed maximum')
    peak = edit_statistics.nonnegative(result['sampled_peak_group_rss'])
    if peak > limits['group_rss_bytes']:
        raise ValueError('supervisor sampled group limit exceeded')
    telemetry_path = bound_path(root, folder/'processes.jsonl')
    proof = result['processes.jsonl']
    if (telemetry_path.stat().st_size != proof['bytes']
            or edit_verify.digest(telemetry_path, 'sha256', 32*MIB) != proof['sha256']):
        raise ValueError('supervisor telemetry changed')
    observed = 0
    actual_peak = 0
    previous = None
    with telemetry_path.open('rb') as stream:
        for value in edit_verify.sample_records(stream, total_limit=32*MIB):
            observed += 1
            now = positive_integer(value['at']['monotonic_ns'])
            if previous is not None and now < previous:
                raise ValueError('supervisor clock moved backwards')
            previous = now
            processes = value['processes']
            if not isinstance(processes, list) or len(processes) > 4:
                raise ValueError('supervisor active-process limit exceeded')
            identities = set()
            total = 0
            for process in processes:
                key = (positive_integer(process['pid']), edit_statistics.nonnegative(process['create_time']))
                if key in identities or process['status'] in ('zombie', 'dead'):
                    raise ValueError('invalid live process observation')
                identities.add(key)
                rss = process['rss']
                if type(rss) is not int or not 0 <= rss <= limits['process_rss_bytes']:
                    raise ValueError('sampled per-process RSS admission exceeded')
                total += rss
            if type(value['total_rss']) is not int or value['total_rss'] != total or total > limits['group_rss_bytes']:
                raise ValueError('sampled group RSS does not reconcile')
            if type(value['free_bytes']) is not int or value['free_bytes'] < limits['free_reserve_bytes']:
                raise ValueError('sampled disk reserve exhausted')
            actual_peak = max(actual_peak, total)
    if observed != count or actual_peak != peak:
        raise ValueError('sample count or peak does not reconcile')
    for name in ('stdout.log', 'stderr.log'):
        capture = result['captures'][name]
        artifact = result[name]
        if (capture['reader_joined'] is not True or capture['eof'] is not True
                or capture['truncated'] is not False or capture['errors'] != []
                or capture['limit_bytes'] != 4*MIB
                or capture['observed_bytes'] != capture['retained_bytes']
                or capture['retained_bytes'] != artifact['bytes']):
            raise ValueError('incomplete supervisor evidence stream')
        path = bound_path(root, folder/name)
        if path.stat().st_size != artifact['bytes'] or edit_verify.digest(path, 'sha256', 4*MIB) != artifact['sha256']:
            raise ValueError('supervisor evidence stream changed')
    begin = positive_integer(start['started']['monotonic_ns'])
    finish = positive_integer(result['finished']['monotonic_ns'])
    if finish < begin or finish-begin > (limits['deadline_seconds']+30)*1_000_000_000:
        raise ValueError('supervisor elapsed/deadline evidence differs')
    return dict(samples=count, sampled_peak_group_rss=actual_peak, root_reaped=True)


def registry(manifest, binding, records):
    expected = edit_disk_budget.complete_cases(manifest)
    if len(expected) != 529 or not same(binding['cases'], expected):
        raise ValueError('frozen registry differs from the entire prospective matrix')
    if hashlib.sha256(canonical(expected)).hexdigest() != binding['cases_sha256']:
        raise ValueError('frozen matrix digest differs')
    ids = [case['id'] for case in expected]
    if not isinstance(records, list) or [record['id'] for record in records] != ids:
        raise ValueError('missing, duplicate, reordered or substituted case record')
    return expected


def required_coverage(request):
    required = PHASE_COVERAGE[request['phase']] | {'sample_identity', 'source_hashes'}
    if request['phase'] == 'correctness':
        if request['fixture_id'].startswith('analytic-'):
            required |= {'analytic_pixels'}
        if request['outputs']:
            required |= {'encoded_pixels_metadata'}
    return required


def case_result(request, receipt, verification, attempts, values):
    if verification.get('complete') is not True or verification.get('error') is not None:
        raise ValueError('independent case verifier failed')
    result = verification['result']
    if result.get('verified') is not True or result.get('whole_story_qualified') is not False:
        raise ValueError('missing or overclaiming independent verification')
    if result.get('remaining', []) != []:
        raise ValueError('case verifier reports unresolved coverage')
    coverage = result['coverage']
    if not isinstance(coverage, list) or any(not isinstance(x, str) for x in coverage) or len(set(coverage)) != len(coverage):
        raise ValueError('invalid independent coverage labels')
    if not required_coverage(request).issubset(coverage):
        raise ValueError('independent verifier lacks required phase coverage')
    if receipt.get('probe_complete') is not True or receipt.get('qualification_complete') is not False or receipt.get('error') is not None:
        raise ValueError('probe incomplete or overclaiming')
    for field in ('phase', 'fixture_id', 'operation', 'source_sha256', 'source_blake3'):
        if not same(receipt[field], request[field]):
            raise ValueError('probe request identity differs')
    edit_verify.sample_coverage(request, attempts, values)
    if type(result['sample_count']) is not int or result['sample_count'] != len(values):
        raise ValueError('verifier observed a different sample count')
    return edit_statistics.summarize_case(request, values)


def deterministic_pairs(case_values):
    """Every real cohort recipe is repeated in separate probe processes."""
    pairs = {}
    for request, values in case_values:
        if request['phase'] != 'correctness' or request['operation'] not in ('all-0', 'all-1'):
            continue
        key = request['fixture_id']
        pair = pairs.setdefault(key, {})
        if request['operation'] in pair:
            raise ValueError('duplicate repeated camera case')
        pair[request['operation']] = (request, values)
    if len(pairs) != 30:
        raise ValueError('missing full-cohort repeat pairs')
    proofs = []
    for fixture, pair in sorted(pairs.items()):
        if set(pair) != {'all-0', 'all-1'}:
            raise ValueError('missing independent repeat')
        (left, a), (right, b) = pair['all-0'], pair['all-1']
        if not same(left['recipes'], right['recipes']) or left['source_sha256'] != right['source_sha256']:
            raise ValueError('repeat recipe/source differs')
        fields = ('width', 'height', 'rgba_f32le_blake3', 'minimum', 'maximum', 'alpha_zero_partial_opaque', 'nonfinite')
        by_identity = lambda rows: {edit_statistics.sample_identity(v, left): {k: v['pixels'][k] for k in fields} for v in rows}
        if not same(by_identity_as_json(by_identity(a)), by_identity_as_json(by_identity(b))):
            raise ValueError('repeated pixel observations differ: '+fixture)
        proofs.append(dict(fixture_id=fixture, recipes=len(left['recipes']), exact_repeat=True))
    return proofs


def by_identity_as_json(values):
    return [[list(key), value] for key, value in sorted(values.items())]


def aggregate(root, binding):
    root = Path(root).resolve(strict=True)
    manifest_path = Path(binding['manifest']['path'])
    if edit_verify.digest(manifest_path, 'sha256', MIB) != binding['manifest']['sha256']:
        raise ValueError('source cohort manifest changed')
    manifest = edit_verify.read_json(manifest_path, MIB)
    records = binding['case_records']
    cases = registry(manifest, binding, records)
    summaries, raw_cases, references, supervisors = [], [], {}, []
    required_sources = {}
    repeat_pids = set()
    actions = {item['id']: item for item in binding['actions']}
    if len(actions) != len(binding['actions']):
        raise ValueError('duplicate frozen action')
    for case, record in zip(cases, records, strict=True):
        case_id = case['id']
        output = bound_path(root, record['probe_output'])
        if output != root/(case_id+'-output'):
            raise ValueError('case output namespace differs')
        request_path = admitted_file(root, record['request_path'], edit_verify.MAX_JSON)
        if request_path != output/'request.json':
            raise ValueError('case request namespace differs')
        request = edit_verify.read_json(request_path)
        if not same(request, record['request']) or not same(request, actions[case_id]['request']):
            raise ValueError('executed request differs from frozen request')
        for field in ('phase', 'fixture_id', 'operation', 'recipes', 'outputs', 'warmups', 'repetitions'):
            if not same(request[field], case[field]):
                raise ValueError('request changed prospective case: '+field)
        receipt = edit_verify.read_json(output/'receipt.json')
        verification_path = admitted_file(root, record['verification_path'], MAX_REPORT)
        if verification_path != root/('verify-'+case_id+'-verification.json'):
            raise ValueError('verifier output namespace differs')
        verification = edit_verify.read_json(verification_path, MAX_REPORT)
        attempts, values = edit_verify.observations(output)
        proof = verification['result']
        hashes = {name: edit_verify.digest(path, 'sha256', limit) for name, path, limit in (
            ('request_sha256', request_path, edit_verify.MAX_JSON),
            ('receipt_sha256', output/'receipt.json', edit_verify.MAX_JSON),
            ('samples_sha256', output/'samples.jsonl', edit_verify.MAX_SAMPLES))}
        if any(proof[key] != value for key, value in hashes.items()):
            raise ValueError('verified request/receipt/samples changed')
        references[case_id] = hashes
        summaries.append(case_result(request, receipt, verification, attempts, values))
        existing = required_sources.setdefault(request['fixture_id'], request)
        if any(not same(existing[key], request[key]) for key in ('source', 'source_sha256', 'source_blake3', 'width', 'height')):
            raise ValueError('cases substituted a different source for one fixture')
        if request['phase'] == 'correctness' and request['operation'] in ('all-0', 'all-1'):
            # PID alone is not identity; spawn creation time is retained below.
            raw_cases.append((request, values))
        for action_id, ref_key in ((case_id, 'probe_supervisor_path'), ('verify-'+case_id, 'verify_supervisor_path')):
            action = actions[action_id]
            limits = {name: action[name] for name in ('deadline_seconds', 'process_rss_bytes', 'group_rss_bytes')}
            limits['free_reserve_bytes'] = binding['free_reserve_bytes']
            # The builder freezes argv after selecting isolated launcher paths.
            supervisors.append(supervisor(root, root/action_id, record[ref_key], action['command'], limits,
                                          receipt['probe_pid'] if action_id == case_id else None))
        if request['phase'] == 'correctness' and request['operation'] in ('all-0', 'all-1'):
            spawn = edit_verify.read_json(root/case_id/'spawn.json')
            identity = (positive_integer(spawn['pid']), edit_statistics.nonnegative(spawn['create_time']))
            if identity in repeat_pids:
                raise ValueError('independent repeats reused one process instance')
            repeat_pids.add(identity)
    for record in records:
        proof = edit_verify.read_json(record['verification_path'], MAX_REPORT)['result']
        for reference in proof.get('reference_cases', []):
            if reference['id'] not in references or not same({k: reference[k] for k in references[reference['id']]}, references[reference['id']]):
                raise ValueError('verifier used an unbound/stale reference case')
    repeat = deterministic_pairs(raw_cases)
    # The coordinator must retain the complete serial action roster; terminal
    # aggregate itself is excluded because this invocation has not yet returned.
    expected_actions = [name for case in cases for name in (case['id'], 'verify-'+case['id'])]
    if [a['id'] for a in binding['actions'] if a['kind'] != 'aggregate'] != expected_actions:
        raise ValueError('incomplete, extra or reordered executed action roster')
    source_proofs = []
    sources = binding['sources']
    if len(sources) != len(required_sources) or {item['id'] for item in sources} != set(required_sources):
        raise ValueError('incomplete source preservation roster')
    for source in sources:
        request = required_sources[source['id']]
        path = Path(source['path'])
        if str(path) != request['source'] or source['sha256'] != request['source_sha256'] or source['blake3'] != request['source_blake3']:
            raise ValueError('source roster differs from actual request')
        limit = request['decode']['max_encoded_bytes']
        for algorithm in ('sha256', 'blake3'):
            if edit_verify.digest(path, algorithm, limit) != source[algorithm]:
                raise ValueError('owned source changed after campaign')
        source_proofs.append(dict(id=source['id'], sha256=source['sha256'], blake3=source['blake3'], unchanged=True))
    missed = [dict(fixture_id=s['fixture_id'], phase=s['phase'], operation=s['operation'], recipe_index=c['recipe_index'], p95_ms=c['elapsed']['p95_ms'], target_ms=c['p95_target_ms'])
              for s in summaries for c in s['configurations'] if c['numeric_target_met'] is False]
    return dict(version=1, complete=True, headless_campaign_qualified=not missed,
                whole_story_qualified=False, case_count=len(cases), cases_sha256=binding['cases_sha256'],
                summaries=summaries, missed_latency_targets=missed, deterministic_repeats=repeat,
                sources=source_proofs, supervisor_count=len(supervisors),
                sampled_peak_group_rss=max(s['sampled_peak_group_rss'] for s in supervisors),
                caveat='Sampled RSS and disk checks are not hard allocation limits; platform/UI delivery remains separate.')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--binding', type=Path, required=True)
    parser.add_argument('--binding-sha256', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result, error = None, None
    try:
        if edit_verify.digest(args.binding, 'sha256', MAX_BINDING) != args.binding_sha256:
            raise ValueError('aggregate binding differs from reviewed identity')
        result = aggregate(args.root, edit_verify.read_json(args.binding, MAX_BINDING))
    except Exception as exc:
        error = type(exc).__name__+': '+str(exc)
    payload = dict(complete=error is None, error=error, result=result)
    encoded = canonical(payload)
    if len(encoded) > MAX_REPORT:
        raise ValueError('aggregate report exceeds fixed byte bound')
    with args.output.open('xb') as stream:
        stream.write(encoded+b'\n')
        stream.flush()
        os.fsync(stream.fileno())
    if error or not result['headless_campaign_qualified']:
        raise SystemExit(error or 'headless campaign misses retained latency targets')


if __name__ == '__main__':
    main()
