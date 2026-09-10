"""Successful Mac worker high-water receipts, distinct from sampled group RSS."""
import math


def resident_bytes(value, limit):
    if type(value) is not int or type(limit) is not int or not 0 < value <= limit:
        raise ValueError('missing or excessive whole-process high-water RSS')
    return value


def worker(value, limit):
    pid = value['pid']
    if type(pid) is not int or pid <= 0:
        raise ValueError('worker memory receipt lacks a process identity')
    method = value['peak_method']
    if not isinstance(method, str) or not 0 < len(method) <= 512 or not method.startswith('getrusage '):
        raise ValueError('worker high-water method is unavailable or unexpected')
    return dict(pid=pid, peak_resident_bytes=resident_bytes(value['peak_resident_bytes'], limit), method=method)


def evidence(request, receipt, values, *, background=None, process_limit=None):
    limit = request['render']['max_live_bytes'] if process_limit is None else process_limit
    peak = receipt['rss']
    if peak.get('status') != 'available':
        raise ValueError('reference-Mac probe high-water RSS unavailable')
    native = resident_bytes(peak['bytes'], limit)
    workers = []
    phase = request['phase']
    if phase in ('warm_service', 'first_raw'):
        for value in values:
            metrics = value['worker_metrics']
            proof = worker(metrics, limit)
            if proof['pid'] not in value['observed_worker_pids'] or value['key'] not in metrics['keys']:
                raise ValueError('preview high-water receipt belongs to a different producer')
            workers.append(proof)
    elif phase in ('export', 'export_correctness'):
        for value in values:
            metrics = value['phases']
            proof = worker(dict(pid=metrics['worker_pid'], peak_resident_bytes=metrics['worker_peak_resident_bytes'],
                                peak_method=metrics['worker_peak_method']), limit)
            launched = {event['event']['pid'] for event in value['events'] if event['event']['state'] == 'started'}
            if launched != {proof['pid']}:
                raise ValueError('export high-water receipt differs from actual launch')
            workers.append(proof)
    elif phase in ('overlap_import', 'overlap_export'):
        if not isinstance(background, dict):
            raise ValueError('background worker high-water evidence missing')
        metrics = background['worker_metrics']
        if not isinstance(metrics, list) or not 1 <= len(metrics) <= 256:
            raise ValueError('bounded background completion metrics required')
        workers = [worker(value, limit) for value in metrics]
        observed = {identity['pid'] for value in values for identity in value['live_workers_before']}
        if not observed.issubset({value['pid'] for value in workers}):
            raise ValueError('overlapping worker has no completed high-water receipt')
    return dict(probe_peak_resident_bytes=native, worker_receipts=workers,
                maximum_worker_peak_resident_bytes=max((value['peak_resident_bytes'] for value in workers), default=None),
                maximum_process_peak_resident_bytes=max([native]+[value['peak_resident_bytes'] for value in workers]),
                process_stop_bytes=limit,
                scope='Process high-water through each successful receipt; not simultaneous group RSS or a cache-hit measurement.')
