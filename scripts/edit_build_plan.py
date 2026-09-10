"""Build a fully resolved, byte-bound prospective S8 execution plan.

Only small prepared receipts are read. This command does not copy/decode sources,
start children, or grant execution. Preparation and build freezing occur first.
"""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import edit_binding
import edit_disk_budget
import edit_fixtures
import edit_qualification as qualification
import edit_request
from edit_verify import read_json,digest


def descriptor(path):
    path=Path(path).resolve(strict=True)
    return dict(path=str(path),sha256=digest(path,'sha256',16*qualification.MIB))


def python_command(build,entry,arguments):
    package=Path(build['helper_package']['root'])
    runtime=build['runtime_binding']
    return [build['python']['path'],'-I','-B',str(package/'scripts/edit_binding.py'),
            '--binding',runtime['path'],'--binding-sha256',runtime['sha256'],
            '--entry',entry,'--',*map(str,arguments)]


def build_plan(preparation,build,manifest,output):
    output=Path(output)
    if not output.is_absolute() or output==Path(preparation['root']):
        raise ValueError('exclusive measured root distinct from preparation required')
    sources=preparation['sources']
    expected_ids=set(qualification.IDS)|set(edit_fixtures.FIXTURES)
    if len(sources)!=len(expected_ids) or {item['id'] for item in sources}!=expected_ids:
        raise ValueError('exact thirty original plus seven generated source roster required')
    by_id={item['id']:item for item in sources}
    cases=edit_disk_budget.complete_cases(manifest)
    funding=edit_disk_budget.budget(manifest)
    normal=qualification.plan(manifest)['normal_limits']
    package=Path(build['helper_package']['root'])
    actions=[]
    records=[]
    for case in cases:
        case_id=case['id']
        request=edit_request.expand_request(case,by_id[case['fixture_id']],normal,output,
            build['worker']['path'],Path(preparation['background_copy']['path']).parent)
        override=case.get('limits',{})
        process=override.get('sampled_worker_rss_stop_bytes',normal['sampled_worker_rss_stop_bytes'])
        group=override.get('sampled_group_rss_stop_bytes',normal['sampled_group_rss_stop_bytes'])
        common=dict(deadline_seconds=case['deadline_seconds'],process_rss_bytes=process,group_rss_bytes=group)
        command=[build['probe']['path'],'--request',str(output/(case_id+'-request.json'))]
        actions.append(dict(id=case_id,kind='probe',request=request,command=command,**common))
        verifier_id='verify-'+case_id
        verification=output/(verifier_id+'-verification.json')
        command=python_command(build,'edit_verify',['--root',request['output'],'--output',verification])
        actions.append(dict(id=verifier_id,kind='verify',probe_output=case_id+'-output',command=command,**common))
        records.append(dict(id=case_id,request_path=str(Path(request['output'])/'request.json'),
            probe_output=request['output'],verification_path=str(verification),
            probe_supervisor_path=str(output/case_id/'result.json'),
            verify_supervisor_path=str(output/verifier_id/'result.json'),request=request,
            cleanup_path=str(output/(case_id+'-cleanup.json')) if case['phase']=='export' else None))
    value=copy.deepcopy(build)
    value.update(version=2,automatic_retries=0,pending_execution_gates=list(build['pending_execution_gates']),
        manifest=preparation['manifest'],preparation_root=preparation['root'],
        preparation_records=preparation['preparation_records'],sources=sources,
        source_copy_receipt=preparation['copy_receipt'],
        background_source=dict(directory=str(Path(preparation['background_copy']['path']).parent),
            path=preparation['background_copy']['path'],fixture_id=preparation['background_copy']['fixture_id'],
            sha256=preparation['background_copy']['sha256'],blake3=preparation['background_copy']['blake3']),
        cases=cases,cases_sha256=hashlib.sha256(qualification.canonical(cases)).hexdigest(),
        normal_limits=normal,actions=actions,case_records=records,funding=funding,
        **{name:funding[name] for name in ('retained_bound_bytes','active_bound_bytes','copies_bound_bytes',
                                          'free_reserve_bytes','minimum_free_bytes')})
    value['verifier']=dict(path=str(package/'scripts/edit_verify.py'),sha256=build['helper_package']['files']['scripts/edit_verify.py'])
    value['fixture_generator']=dict(path=str(package/'scripts/edit_fixtures.py'),sha256=build['helper_package']['files']['scripts/edit_fixtures.py'])
    # Cleanup may be admitted only with its reviewed exact target map; leave it
    # blocked until the caller supplies the completed preparation/build decision.
    return value


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--preparation',type=Path,required=True)
    parser.add_argument('--build',type=Path,required=True)
    parser.add_argument('--campaign-root',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    preparation=read_json(args.preparation,16*qualification.MIB)
    build=read_json(args.build,16*qualification.MIB)
    manifest_path=preparation['manifest']['path']
    if digest(manifest_path,'sha256',qualification.MIB)!=preparation['manifest']['sha256']:
        raise ValueError('original cohort manifest bytes differ')
    result=build_plan(preparation,build,read_json(manifest_path,qualification.MIB),args.campaign_root)
    encoded=json.dumps(result,indent=2,allow_nan=False)+'\n'
    if len(encoded.encode())>16*qualification.MIB:
        raise ValueError('resolved binding exceeds16MiB; revise preparation before any campaign')
    with args.output.open('x') as stream:
        stream.write(encoded)

if __name__=='__main__':main()
