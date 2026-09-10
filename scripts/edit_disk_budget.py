"""Prospective content/overhead funding. No filesystem or workload operations."""
from __future__ import annotations
import math
import edit_qualification as q
import edit_correctness_matrix as supplementary
import edit_fixtures


def complete_cases(manifest):
    base=q.plan(manifest)['cases']
    # Current-delivery verifiers consume these full reference cases immediately.
    return (supplementary.proxy_references()+base+supplementary.analytic_matrix()+supplementary.output_matrix()
            +supplementary.support_matrix()
            +supplementary.failures()+supplementary.durable_metadata_cases()
            +supplementary.overlap_cases())


def outer_owner():
    return dict(deadline_seconds=86400,
        supervision=dict(max_active=8,max_seen=131072,max_telemetry_bytes=8*q.GIB,
                         max_sample_bytes=8192,max_identity_bytes=128*q.MIB,max_identity_event_bytes=512),
        host_logs=dict(max_bytes=8*q.GIB,max_record_bytes=q.MIB),
        stdout_bytes=4*q.MIB,stderr_bytes=4*q.MIB,
        caveat='24-hour overall failure stop, below summed action ceilings; no automatic retry or qualification')


def preparation_owner():
    return dict(deadline_seconds=3900,
        supervision=dict(max_active=4,max_seen=8192,max_telemetry_bytes=128*q.MIB,
                         max_sample_bytes=8192,max_identity_bytes=8*q.MIB,max_identity_event_bytes=512),
        host_logs=dict(max_bytes=512*q.MIB,max_record_bytes=q.MIB),
        stdout_bytes=4*q.MIB,stderr_bytes=4*q.MIB,
        caveat='Untimed preparation outer failure stop; source-copy and generator limits unchanged')


def budget(manifest):
    cases=complete_cases(manifest)
    dimensions={i['id']:(i['width'],i['height']) for i in manifest['inputs']}
    dimensions.update(edit_fixtures.FIXTURES)
    retained_raw=0
    retained_encoded=0
    retained_proxy_reference=0
    namespace_count=0
    active_extra=0
    files=0
    normal_extent=512*q.MIB
    per_namespace_payload=(64+256+256)*q.MIB  # retained, large, prepared
    per_namespace_sql_allowance=64*q.MIB
    per_namespace_entries=1024
    for case in cases:
        phase=case['phase']
        w,h=dimensions[case['fixture_id']]
        extent=case.get('limits',{}).get('encoded_extent',case.get('encoded_extent',normal_extent))
        count=len(case['recipes'])
        if phase in ('correctness','support100mp','large_cancellation'):
            raw_needed=(case['operation']=='combined' or w*h<=512*512)
            if raw_needed:
                retained_raw+=w*h*16*count
                files+=count
            retained_encoded+=extent*len(case['outputs'])*count
            files+=len(case['outputs'])*count
        elif phase=='proxy_reference':
            scale=min(1,1600/max(w,h))
            pw=max(1,math.floor(w*scale+.5))
            ph=max(1,math.floor(h*scale+.5))
            retained_raw+=pw*ph*16*count
            retained_proxy_reference+=8*q.MIB*count
            files+=2*count
        if phase in ('warm_service','first_raw','export','export_correctness','overlap_import','overlap_export'):
            namespace_count+=1
            files+=per_namespace_entries
            if phase=='export':
                # Keep first measured complete output; additional21 successful
                # destinations plus an additional sealed copy can coexist with current staging;
                # all are funded until independent verification and cleanup.
                retained_encoded+=extent
                active_extra=max(active_extra,(case['warmups']+case['repetitions'])*extent)
                files+=1
            elif phase=='export_correctness':
                retained_encoded+=extent
                files+=1
            elif phase=='overlap_export':
                retained_encoded+=extent
                files+=1
            elif phase in ('warm_service','first_raw'):
                retained_encoded+=len(case['recipes'])*8*q.MIB
                retained_raw+=len(case['recipes'])*1600*1600*3
                files+=len(case['recipes'])
                files+=len(case['recipes'])
    # Two children per probe (probe+independent verification), seven generators,
    # then one aggregate. This must be revised if actual actions change.
    children=2*len(cases)+len(edit_fixtures.FIXTURES)+1
    stream_allowance=children*(40*q.MIB+256*1024)  # 32MiB samples +4MiB per stream +256KiB lifetime events
    outer=outer_owner()
    outer_allowance=(outer["supervision"]["max_telemetry_bytes"]+outer["supervision"]["max_identity_bytes"]
                     +outer["host_logs"]["max_bytes"]+outer["stdout_bytes"]+outer["stderr_bytes"]+q.MIB)
    preparation=preparation_owner()
    preparation_allowance=(preparation['supervision']['max_telemetry_bytes']+preparation['supervision']['max_identity_bytes']
                           +preparation['host_logs']['max_bytes']+preparation['stdout_bytes']+preparation['stderr_bytes']+q.MIB)
    request_receipt_allowance=len(cases)*20*q.MIB
    namespace_allowance=namespace_count*(per_namespace_payload+per_namespace_sql_allowance)
    allocation_overhead=(files+children*9+32)*4096
    retained=retained_raw+retained_encoded+retained_proxy_reference+namespace_allowance+stream_allowance+request_receipt_allowance+allocation_overhead+outer_allowance+preparation_allowance
    # Owned originals use declared encoded ceilings, never expected compression.
    copies=len(manifest['inputs'])*normal_extent+4*q.GIB+16*q.MIB+normal_extent
    active=active_extra+32*q.MIB # native staging reservation, beyond retained basis
    reserve=16*q.GIB
    funded=retained+active+copies+reserve
    minimum=math.ceil(funded/q.GIB)*q.GIB
    return dict(version=2,outer_owner=outer,preparation_owner=preparation,proposed_probe_count=len(cases),proposed_total_children=children,
        components=dict(raw=retained_raw,encoded=retained_encoded,proxy_jpeg=retained_proxy_reference,
                        service_namespaces=namespace_allowance,evidence_streams=stream_allowance,outer_and_host=outer_allowance,preparation_outer_and_host=preparation_allowance,
                        requests_receipts=request_receipt_allowance,allocation_overhead=allocation_overhead),
        retained_bound_bytes=retained,active_bound_bytes=active,copies_bound_bytes=copies,
        free_reserve_bytes=reserve,minimum_free_bytes=minimum,
        output_stop_bytes=retained+active,
        accounting_scope='content extents plus explicit sampled metadata/filesystem allowances; live free-space guard remains required',
        retention='all failures and camera correctness outputs; each successful export timing child keeps its first measured output after all22 independent readbacks',
        pending='final action/verification sizes must equal this count; measured filesystem overhead and live guards do not guarantee immunity to external disk consumption')
