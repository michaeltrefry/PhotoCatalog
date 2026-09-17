#!/usr/bin/env python3
"""All-input durable import timing gate; terminal/data reconciliation stays separate."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

from observe_import_native_v4 import BINDINGS, CLOCK, PROTOCOL, ROLES, declaration_digest, require, validate_declaration

COUNTERS = ('imported', 'unchanged', 'failed', 'skipped', 'metadata_updated', 'metadata_warnings', 'awaiting_resources')
PROGRESS = ('imported', 'unchanged', 'failed', 'skipped', 'metadata_updated')
ACTIVE = ('discovering', 'draining', 'cancel_requested')
PHASES = (*ACTIVE, 'complete', 'canceled', 'failed')
MAX_EVENT = 3002 * 2 + 512 * 3 + 1202 * 2


def natural(value):
    return type(value) is int and value >= 0


def native_segments(rows, declaration):
    require(len(rows) >= 2 and rows[0].get('kind') == 'identity' and rows[-1].get('kind') == 'summary', 'Incomplete observer')
    identity, summary = rows[0], rows[-1]
    allowed = {'identity', 'summary', 'waiting', 'segment_admitted', 'active', 'segment_closed', 'observation_gap', 'error'}
    require(all(row.get('kind') in allowed for row in rows), 'Unknown observer row')
    require(sum(r['kind'] == 'identity' for r in rows) == sum(r['kind'] == 'summary' for r in rows) == 1, 'Repeated observer boundary')
    require(identity['protocol'] == summary['protocol'] == PROTOCOL and identity['clock'] == summary['clock'] == CLOCK
            and identity['profile'] == 'import_v1' and identity['clock_implementation'] == 'mach_absolute_time()', 'Observer clock/profile')
    require(all(identity[key] == declaration[key] for key in BINDINGS) and identity['declaration_sha256'] == declaration_digest(declaration), 'Observer binding mismatch')
    require(0 < identity['seconds'] <= 600 and .05 <= identity['interval'] <= 1, 'Observer configuration bounds')
    require(summary['fatal_errors'] == 0 and not any(r['kind'] == 'error' for r in rows), 'Observer errors')
    require(summary['root_same_birth'] is True and summary['executable_unchanged'] is True, 'Observer identity changed')
    first, end = identity['monotonic_ns'], summary['measurement_end_monotonic_ns']
    require(natural(first) and natural(end) and first <= end, 'Observer time bounds')
    positives = [r for r in rows if r['kind'] in ('segment_admitted', 'active')]
    closes = [r for r in rows if r['kind'] == 'segment_closed']
    gaps = [r for r in rows if r['kind'] == 'observation_gap']
    segments = summary['segments']
    require(len(segments) <= 1 and [s['segment'] for s in segments] == list(range(1, len(segments) + 1)), 'Reopened native segment')
    require({r['segment'] for r in positives} == {s['segment'] for s in segments}
            and len(closes) == len(segments) and {r['segment'] for r in closes} == {s['segment'] for s in segments}, 'Raw/summary bijection')
    require(summary['positive_observations'] == len(positives) and summary['observation_gaps'] == len(gaps), 'Raw/summary count mismatch')
    result, previous = [], first
    for row in rows[1:-1]:
        timestamp = row.get('positive_after_monotonic_ns', row.get('monotonic_ns'))
        require(natural(timestamp) and previous <= timestamp <= end, 'Raw chronological bounds')
        previous = timestamp
    for segment in segments:
        require(positives[0]['kind'] == 'segment_admitted' and all(r['kind'] == 'active' for r in positives[1:]), 'Raw segment admission/reopen')
        require(segment['positive_observations'] == len(positives), 'Segment positive count mismatch')
        last_after = first
        for row in positives:
            before, after = row['positive_before_monotonic_ns'], row['positive_after_monotonic_ns']
            require(natural(before) and last_after <= before <= after <= end, 'Positive probe ordering')
            last_after = after
            require(row['lock_identity'] == declaration['lock_identity'], 'Wrong lock identity')
            require(row['lock_contended_before'] is True and row['lock_contended_after'] is True, 'Missing exclusive contention proof')
            require(row['role_owners'] == {role: [{'pid': declaration[f'{role}_pid'], 'birth_unix_s': declaration[f'{role}_birth_unix_s']}] for role in ROLES}, 'Wrong declared GUI role bindings')
            require(set(row['processes']) == {'root', 'desktop', 'filesystem'}, 'Missing process proof')
            for role, process in row['processes'].items():
                require(process['pid'] == declaration[f'{role}_pid'] and process['birth_unix_s'] == declaration[f'{role}_birth_unix_s'], 'Process birth/identity mismatch')
                argv = process['argv']
                require(isinstance(argv, list) and bool(argv) and argv[0] == declaration['executable'], 'Wrong process executable')
                require(process['executable_identity'] == declaration['executable_identity'], 'Wrong process executable object')
                if role != 'root':
                    require(process['parent_pid'] == declaration['root_pid'] and argv[1:] == [ROLES[role]], 'Wrong native role/ancestry')
                else:
                    require(not any(arg in ROLES.values() for arg in argv[1:]), 'Root is worker')
            holder = row['holder']
            require(holder['pid'] == declaration['filesystem_pid'] and holder['path'] == str(Path(declaration['catalog']) / 'import.lock')
                    and holder['device_inode'] == declaration['lock_identity'] and holder['sole_visible_owner'] is True
                    and 0 < len(holder['descriptors']) <= 128
                    and len({d['fd'] for d in holder['descriptors']}) == len(holder['descriptors'])
                    and all(isinstance(d['fd'], str) and d['fd'].isdigit() and d['lock_field'] in (' ', 'W') for d in holder['descriptors'])
                    and natural(holder['elapsed_ns']) and holder['elapsed_ns'] <= 1_000_000_000, 'Wrong lsof holder proof')
        close = closes[0]
        require(close['reason'] == segment['closed_reason'] and close['monotonic_ns'] == segment['closed_monotonic_ns']
                and rows.index(close) > rows.index(positives[-1]) and last_after <= close['monotonic_ns'] <= end, 'Closure boundary mismatch')
        if close['reason'] == 'observer_end':
            require(close['monotonic_ns'] == end and not gaps, 'Observer-end closure mismatch')
        else:
            require(close['reason'] == 'lock_not_contended' and len(gaps) == 1 and gaps[0]['reason'] == close['reason']
                    and gaps[0]['monotonic_ns'] == close['monotonic_ns'] and rows.index(gaps[0]) == rows.index(close) + 1, 'Missing raw closure cause')
        require(not any(r['kind'] in ('waiting', 'observation_gap') for r in rows[rows.index(positives[0]):rows.index(positives[-1]) + 1]), 'Gap bridged by native segment')
        lo = positives[0]['positive_after_monotonic_ns']
        hi = positives[-1]['positive_before_monotonic_ns'] if len(positives) > 1 else None
        require(segment['first_positive_after_monotonic_ns'] == lo and segment['last_positive_before_monotonic_ns'] == hi, 'Raw/summary endpoint mismatch')
        if hi is not None:
            require(lo <= hi, 'Inverted native segment')
            result.append((lo, hi))
    require(summary['usable_segments'] == len(result), 'Usable segment count mismatch')
    require(bool(segments) or not gaps, 'Gap without admitted segment')
    return result


def evaluate(receipt, rows, declaration):
    validate_declaration(declaration)
    segments = native_segments(rows, declaration)
    require(receipt['protocol'] == 2 and receipt['run_id'] == declaration['run_id'] and receipt['overflowed'] == 0, 'Receipt binding/overflow')
    cutoff, expected = declaration['setup_cutoff_ordinal'], declaration['input_ordinals']
    require(type(cutoff) is int and 0 <= cutoff <= 412 and isinstance(expected, list) and all(type(i) is int for i in expected) and expected == list(range(cutoff + 1, cutoff + 101)), 'Exactly 100 contiguous predeclared inputs required')
    require(declaration['input_kind'] in ('cull', 'edit') and declaration['input_action'] == ('rating' if declaration['input_kind'] == 'cull' else 'edit'), 'Rating or edit cohort required')
    values = receipt['samples']
    require(len(values) <= 512 and all(type(s['ordinal']) is int for s in values) and [s['ordinal'] for s in values] == list(range(1, len(values) + 1))
            and cutoff <= len(values) <= cutoff + 100, 'Missing setup/duplicate/extra/undeclared samples')
    samples = {s['ordinal']: s for s in values}
    require(all(s['kind'] == declaration['input_kind'] for i, s in samples.items() if i > cutoff), 'Wrong input kind')
    alignment = receipt['clock_alignment']
    require(alignment['model'] == 'causal_native_brackets_v1' and alignment['profile'] == 'import_v1'
            and alignment['interval_ms'] == 200 and alignment['duration_ms'] == 600000, 'Unqualified import bridge')
    require(alignment['stop_reason'] in ('finalized', 'duration_elapsed', 'anchor_limit') and len(alignment['anchors']) <= 3002, 'Incomplete/failed/overflowed bridge')
    seen, anchors, prior_receive, prior_ns, session = set(), [], 0, 0, None

    def event(value):
        require(type(value) is int and 0 < value <= MAX_EVENT and value not in seen, 'Invalid/duplicate causal event')
        seen.add(value)

    for index, anchor in enumerate(alignment['anchors'], 1):
        require(anchor['anchor_id'] == index and anchor['send_event'] > prior_receive, 'Anchor sequence/overlap')
        event(anchor['send_event'])
        receive = anchor['receive_event']
        if receive is None:
            require(index == len(alignment['anchors']) and anchor['native'] is None and anchor['error'] == 'incomplete', 'Malformed pending anchor')
            continue
        event(receive)
        require(receive > anchor['send_event'] and anchor['error'] is None, 'Reversed/failed anchor')
        prior_receive = receive
        native = anchor['native']
        require(native['run_id'] == receipt['run_id'] and native['anchor_id'] == index and native['clock'] == CLOCK
                and native['native_pid'] == declaration['root_pid'], 'Native anchor identity mismatch')
        ns = native['monotonic_ns']
        require(isinstance(ns, str) and ns.isascii() and ns.isdigit() and len(ns) <= 20 and 0 < int(ns) <= 2**64 - 1
                and int(ns) >= prior_ns, 'Invalid/lossy/reversed native timestamp')
        require(isinstance(native['session_id'], str) and 0 < len(native['session_id']) <= 256
                and (session is None or session == native['session_id']), 'Native session changed')
        session, prior_ns = native['session_id'], int(ns)
        anchors.append((anchor['send_event'], receive, int(ns)))
    sample_events = {}
    for row in alignment['sample_events']:
        ordinal = row['ordinal']
        require(ordinal in samples and ordinal not in sample_events, 'Duplicate/unbound sample event')
        event(row['start_event'])
        end, durable = row.get('end_event'), row.get('durable_event')
        if end is not None:
            event(end)
            require(end > row['start_event'], 'Reversed sample')
        if durable is not None:
            event(durable)
            require(row['start_event'] < durable and (end is None or durable < end), 'Reversed durable event')
            require(samples[ordinal].get('durable_us') is not None, 'Durable event without latency')
        if samples[ordinal].get('import_id') is not None:
            require((durable is not None) == (samples[ordinal].get('durable_us') is not None), 'Durable event/timing mismatch')
        # Non-import setup may precede or follow profile activation, so its
        # durable event is optional. Present events still validate above.
        sample_events[ordinal] = row
    require(set(sample_events) == set(samples), 'Missing sample causal event')
    require([r['ordinal'] for r in alignment['sample_events']] == list(samples)
            and all(a['start_event'] < b['start_event'] for a, b in zip(alignment['sample_events'], alignment['sample_events'][1:])), 'Sample causal start order')
    evidence = alignment['import_evidence']
    require(evidence['overflowed'] == 0 and len(evidence['bindings']) == 1 and len(evidence['timeline']) <= 1202, 'Import evidence overflow/unbound campaign')
    require(evidence['bindings'][0] == {'key': 1, 'id': declaration['import_id'], 'source_blake3': declaration['source_blake3']}, 'Wrong import UUID/source binding')
    timeline, previous_event = evidence['timeline'], 0
    for row in timeline:
        event(row['request_event'])
        event(row['event'])
        require(previous_event < row['request_event'] < row['event'] and row['binding'] == 1 and row['phase'] in PHASES, 'Invalid import status sequence/binding/phase')
        previous_event = row['event']
        require(natural(row['pending_previews']) and row['pending_previews'] <= 2**32 - 1, 'Invalid pending previews')
        for field in COUNTERS:
            value = row[field]
            require(isinstance(value, str) and value.isascii() and value.isdigit() and len(value) <= 20
                    and str(int(value)) == value and int(value) <= 2**64 - 1, 'Invalid status counter')
    for left, right in zip(timeline, timeline[1:]):
        require(all(int(left[field]) <= int(right[field]) for field in PROGRESS), 'Import progress reversed')
        require(left['phase'] not in ('complete', 'canceled', 'failed') or right['phase'] == left['phase'], 'Import terminal phase reversed')

    def envelope(start, finish):
        before = [a for a in anchors if a[1] < start]
        after = [a for a in anchors if a[0] > finish]
        if not before or not after:
            return None
        lower, upper = before[-1][2], after[0][2]
        if any(lo <= lower <= upper <= hi for lo, hi in segments):
            return {'lower_ns': str(lower), 'upper_ns': str(upper), 'segment': 1}
        return None

    decisions, active_failures, unproven_failures = [], [], []
    for ordinal in expected:
        sample, ev = samples.get(ordinal), sample_events.get(ordinal)
        full, start = None, None
        if sample is not None:
            require(sample['during_import'] is True and sample.get('import_id') == declaration['import_id'], 'Sample import context mismatch')
            start = envelope(ev['start_event'], ev['start_event'])
            if sample.get('durable_us') is not None:
                require(natural(sample['durable_us']), 'Invalid durable latency')
                full = envelope(ev['start_event'], ev['durable_event'])
            if sample['outcome'] != 'complete':
                (active_failures if start else unproven_failures).append(ordinal)
            else:
                require(sample.get('durable_us') is not None and ev.get('end_event') is not None, 'Complete input lacks acknowledgment/end')
        decisions.append({'ordinal': ordinal, 'outcome': sample['outcome'] if sample else 'missing', 'durable_envelope': full, 'start_envelope': start})
    first_event = sample_events.get(expected[0], {}).get('start_event')
    last_event = sample_events.get(expected[-1], {}).get('durable_event')
    # Status context and useful progress are independent from input latency.
    context = bool(first_event and last_event and any(r['event'] < first_event and r['phase'] in ACTIVE for r in timeline)
                   and any(first_event < r['request_event'] < r['event'] < last_event and r['phase'] in ACTIVE for r in timeline)
                   and any(r['request_event'] > last_event for r in timeline))
    progress = []
    if first_event and last_event:
        for left, right in zip(timeline, timeline[1:]):
            if (first_event <= left['request_event'] < left['event'] < right['request_event'] < right['event'] <= last_event and left['phase'] in ACTIVE and right['phase'] in ACTIVE
                    and any(int(right[f]) > int(left[f]) for f in PROGRESS) and envelope(left['request_event'], right['event'])):
                progress.append({'first_request_event': left['request_event'], 'first_response_event': left['event'],
                                 'second_request_event': right['request_event'], 'second_response_event': right['event']})
    result = {'protocol': PROTOCOL, 'run_id': receipt['run_id'], 'import_id': declaration['import_id'], 'cohort': expected,
              'decisions': decisions, 'proven_active_failures': active_failures, 'unproven_failures': unproven_failures,
              'status_context_complete': context, 'native_progress_event_pairs': progress,
              'scope': 'Timing only. Terminal import counts, exact corpus/source invariance, final rating/recipe state and checked worker drain remain required.'}
    if active_failures:
        return {**result, 'verdict': 'FAILED_ACTIVE_INPUT'}
    if any(d['outcome'] != 'complete' or d['durable_envelope'] is None for d in decisions) or unproven_failures or not context or not progress:
        return {**result, 'verdict': 'PARTIAL'}
    import numpy as np
    latencies = [samples[i]['durable_us'] / 1000 for i in expected]
    p95 = float(np.percentile(latencies, 95, method='linear'))
    return {**result, 'durable_p95_ms': p95, 'durable_max_ms': max(latencies),
            'verdict': 'TIMING_PASS_REQUIRES_IMPORT_RECONCILIATION' if p95 <= 100 else 'FAILED_LATENCY'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('receipt', 'observer', 'declaration', 'output'):
        parser.add_argument('--' + name, required=True, type=Path)
    args = parser.parse_args()
    paths = {key: getattr(args, key) for key in ('receipt', 'observer', 'declaration')}
    raw = {}
    try:
        raw = {key: path.read_bytes() for key, path in paths.items()}
        result = evaluate(json.loads(raw['receipt']), [json.loads(line) for line in raw['observer'].splitlines()], json.loads(raw['declaration']))
    except (ValueError, KeyError, TypeError, IndexError, OSError) as error:
        result = {'verdict': 'INVALID_EVIDENCE', 'error': f'{type(error).__name__}: {error}'}
    result['inputs'] = {key: {'path': str(path), 'sha256': hashlib.sha256(raw[key]).hexdigest() if key in raw else None} for key, path in paths.items()}
    with args.output.open('x') as output:
        json.dump(result, output, indent=2)
        output.write('\n')
    return 0 if result['verdict'] == 'TIMING_PASS_REQUIRES_IMPORT_RECONCILIATION' else 2


if __name__ == '__main__':
    raise SystemExit(main())
