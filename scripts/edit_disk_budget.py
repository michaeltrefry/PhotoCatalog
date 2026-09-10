"""Prospective content/overhead funding. No filesystem or workload operations."""
from __future__ import annotations
import math
import edit_qualification as q
import edit_correctness_matrix as supplementary
import edit_fixtures


def complete_cases(manifest):
    base=q.plan(manifest)['cases']
    return (base+supplementary.analytic_matrix()+supplementary.output_matrix()
            +supplementary.proxy_references()+supplementary.support_matrix()
            +supplementary.failures()+supplementary.durable_metadata_cases()
            +supplementary.overlap_cases())


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
        if phase in ('correctness','support100mp'):
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
                # objects exist simultaneously until the whole child is verified.
                retained_encoded+=extent
                active_extra=max(active_extra,(case['warmups']+case['repetitions']-1)*extent)
                files+=1
            elif phase=='export_correctness':
                retained_encoded+=extent
                files+=1
            elif phase=='overlap_export':
                retained_encoded+=extent
                files+=1
            elif phase in ('warm_service','first_raw'):
                retained_encoded+=len(case['recipes'])*8*q.MIB
                files+=len(case['recipes'])
    # Two children per probe (probe+independent verification), six generators,
    # then one aggregate. This must be revised if actual actions change.
    children=2*len(cases)+len(edit_fixtures.FIXTURES)+1
    stream_allowance=children*40*q.MIB  # 32MiB telemetry +4MiB each stdout/stderr
    request_receipt_allowance=len(cases)*20*q.MIB
    namespace_allowance=namespace_count*(per_namespace_payload+per_namespace_sql_allowance)
    allocation_overhead=(files+children*8)*4096
    retained=retained_raw+retained_encoded+retained_proxy_reference+namespace_allowance+stream_allowance+request_receipt_allowance+allocation_overhead
    # Owned originals use declared encoded ceilings, never expected compression.
    copies=len(manifest['inputs'])*normal_extent+2*q.GIB+16*q.MIB+normal_extent
    active=active_extra+32*q.MIB # native staging reservation, beyond retained basis
    reserve=16*q.GIB
    funded=retained+active+copies+reserve
    minimum=math.ceil(funded/q.GIB)*q.GIB
    return dict(version=1,proposed_probe_count=len(cases),proposed_total_children=children,
        components=dict(raw=retained_raw,encoded=retained_encoded,proxy_jpeg=retained_proxy_reference,
                        service_namespaces=namespace_allowance,evidence_streams=stream_allowance,
                        requests_receipts=request_receipt_allowance,allocation_overhead=allocation_overhead),
        retained_bound_bytes=retained,active_bound_bytes=active,copies_bound_bytes=copies,
        free_reserve_bytes=reserve,minimum_free_bytes=minimum,
        output_stop_bytes=retained+active,
        accounting_scope='content extents plus explicit sampled metadata/filesystem allowances; live free-space guard remains required',
        retention='all failures and camera correctness outputs; each successful export timing child keeps its first measured output after all22 independent readbacks',
        pending='final action/verification sizes must equal this count; measured filesystem overhead and live guards do not guarantee immunity to external disk consumption')
