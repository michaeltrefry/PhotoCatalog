"""Fail-closed final registry, imported code and prepared-source admission."""
import sys
import os
import itertools
from pathlib import Path
import edit_aggregate
import edit_binding
import edit_build_plan
import edit_disk_budget
import edit_fixtures
import edit_qualification
import edit_request
from edit_verify import digest,read_json,owned


def validate_record_paths(record,root,case):
    case_id=case['id']
    output=Path(root)/(case_id+'-output')
    expected=dict(probe_output=str(output),request_path=str(output/'request.json'),
        verification_path=str(Path(root)/('verify-'+case_id+'-verification.json')),
        probe_supervisor_path=str(Path(root)/case_id/'result.json'),
        verify_supervisor_path=str(Path(root)/('verify-'+case_id)/'result.json'),
        cleanup_path=str(Path(root)/(case_id+'-cleanup.json')) if case['phase']=='export' else None)
    if any(record.get(name)!=value for name,value in expected.items()):
        raise ValueError('case receipt/cleanup path differs from its owned namespace')


def validate_funding(binding,funding):
    if not edit_aggregate.same(binding.get('funding'),funding):
        raise ValueError('funded peak differs from complete registry')
    for name in ('retained_bound_bytes','active_bound_bytes','copies_bound_bytes','free_reserve_bytes','minimum_free_bytes'):
        if type(binding.get(name)) is not int or binding[name]!=funding[name]:
            raise ValueError('top-level funding differs')
    for owner in ('outer_owner','preparation_owner'):
        if not edit_aggregate.same(binding.get(owner),funding[owner]):
            raise ValueError(owner+' evidence admission differs')


def validate_execution(binding):
    if not sys.flags.isolated or not sys.flags.dont_write_bytecode:
        raise ValueError('qualification coordinator requires isolated no-bytecode-write launcher')
    package=edit_binding.admit_imports(binding['helper_package'])
    edit_binding.validate_runtime(binding['python_runtime'])
    runtime=binding['runtime_binding']
    if digest(runtime['path'],'sha256',16*1024*1024)!=runtime['sha256']:
        raise ValueError('runtime binding file differs')
    recorded=read_json(runtime['path'],16*1024*1024)
    if recorded['helper_package']!=binding['helper_package'] or recorded['python_runtime']!=binding['python_runtime']:
        raise ValueError('launcher and coordinator dependency bindings differ')
    if digest(binding['manifest']['path'],'sha256',1024*1024)!=binding['manifest']['sha256']:
        raise ValueError('original cohort manifest differs')
    manifest=read_json(binding['manifest']['path'],1024*1024)
    cases=edit_disk_budget.complete_cases(manifest)
    edit_binding.verify_case_registry(binding['cases_sha256'],cases)
    if not edit_aggregate.same(binding['cases'],cases):raise ValueError('frozen case contents differ')
    funding=edit_disk_budget.budget(manifest)
    validate_funding(binding,funding)
    normal=edit_qualification.plan(manifest)['normal_limits']
    if not edit_aggregate.same(binding['normal_limits'],normal):raise ValueError('normal resource profile differs')
    expected=set(edit_qualification.IDS)|set(edit_fixtures.FIXTURES)
    sources=binding['sources']
    if len(sources)!=len(expected) or {source['id'] for source in sources}!=expected:
        raise ValueError('prepared source roster differs')
    by_id={source['id']:source for source in sources}
    originals={source['id']:source for source in manifest['inputs']}
    preparation=Path(binding['preparation_root']).resolve(strict=True)
    for source in sources:
        path=owned(preparation,source['path'])
        limit=2*1024**3 if source['id'] in edit_fixtures.FIXTURES else normal['decode']['max_encoded_bytes']
        if digest(path,'sha256',limit)!=source['sha256'] or digest(path,'blake3',limit)!=source['blake3']:
            raise ValueError('unbound or changed prepared source bytes')
        if source['id'] in originals:
            original=originals[source['id']]
            if source['original_path']!=original['path'] or any(source[k]!=original[k] for k in ('sha256','width','height')):
                raise ValueError('original source-copy association differs')
        else:
            generated=source['generated_receipt']
            receipt_path=owned(preparation,generated['path'])
            if digest(receipt_path,'sha256',256*1024)!=generated['sha256']:raise ValueError('generated receipt differs')
            receipt=read_json(receipt_path)
            if any(receipt[k]!=source[k] for k in ('id','path','sha256','blake3','width','height')):
                raise ValueError('generated source bytes lack exact construction receipt')
    background=binding['background_source']
    original=originals[background['fixture_id']]
    background_path=owned(preparation,background['path'])
    directory=owned(preparation,background['directory'])
    if background_path.parent!=directory or directory==Path(by_id[background['fixture_id']]['path']).parent:
        raise ValueError('background source must be a separate owned import directory')
    with os.scandir(directory) as entries:
        actual=list(itertools.islice(entries,2))
    if len(actual)!=1 or actual[0].path!=str(background_path) or not actual[0].is_file(follow_symlinks=False):
        raise ValueError('background directory must contain exactly one ordinary file')
    if background['sha256']!=original['sha256'] or digest(background_path,'sha256',normal['decode']['max_encoded_bytes'])!=background['sha256'] or digest(background_path,'blake3',normal['decode']['max_encoded_bytes'])!=background['blake3']:
        raise ValueError('background copied source differs')
    records=binding['case_records']
    if len(records)!=len(cases) or len(binding['actions'])!=2*len(cases):
        raise ValueError('complete paired action/record coverage required')
    root=Path(records[0]['probe_output']).parent
    for index,(case,record) in enumerate(zip(cases,records,strict=True)):
        validate_record_paths(record,root,case)
        expected_request=edit_request.expand_request(case,by_id[case['fixture_id']],normal,root,
            binding['worker']['path'],background['directory'])
        if record['id']!=case['id'] or not edit_aggregate.same(record['request'],expected_request):
            raise ValueError('resolved request differs from frozen source/case/defaults')
        probe,verifier=binding['actions'][2*index:2*index+2]
        if probe['id']!=case['id'] or verifier['id']!='verify-'+case['id'] or probe['kind']!='probe' or verifier['kind']!='verify':
            raise ValueError('reference-safe serial action order differs')
        if not edit_aggregate.same(probe['request'],expected_request):raise ValueError('action request differs')
        expected_probe=[binding['probe']['path'],'--request',str(root/(case['id']+'-request.json'))]
        expected_verifier=edit_build_plan.python_command(binding,'edit_verify',[
            '--root',expected_request['output'],'--output',root/('verify-'+case['id']+'-verification.json')])
        override=case.get('limits',{})
        limits=dict(deadline_seconds=case['deadline_seconds'],
            process_rss_bytes=override.get('sampled_worker_rss_stop_bytes',normal['sampled_worker_rss_stop_bytes']),
            group_rss_bytes=override.get('sampled_group_rss_stop_bytes',normal['sampled_group_rss_stop_bytes']))
        for action in (probe,verifier):
            if any(action[name]!=value for name,value in limits.items()):
                raise ValueError('case process/deadline admission differs')
        if probe['command']!=expected_probe or verifier['command']!=expected_verifier:
            raise ValueError('frozen action argv differs')
        if record['probe_output']!=expected_request['output']:
            raise ValueError('case output ownership differs')
    # Verify imports actually originate in the sealed scripts package. Looking at
    # a source archive alone cannot establish what this interpreter imported.
    for name in edit_binding.HELPERS:
        module=sys.modules.get(name)
        if module is not None and Path(module.__file__).resolve()!=package/'scripts'/(name+'.py'):
            raise ValueError('actual imported helper escaped frozen package')
