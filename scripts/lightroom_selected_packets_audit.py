"""Quiescent selected-PACKETS metadata audit; never a migration/acceptance grant.

Recorded PAGE digests are attributed claims. This does not independently read
packet/page/source bodies or prove their semantic completeness.
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import time


def require(condition, message):
    if not condition: raise ValueError(message)


@contextlib.contextmanager
def metadata_budget(contract, maximum=128*1024**2):
    original = contract.raw; used = 0
    def bounded(path, cap=contract.CAP):
        nonlocal used
        remaining = maximum-used
        require(remaining > 1, 'audit metadata budget')
        data = original(path, min(cap, remaining-1)); used += len(data)
        return data
    contract.raw = bounded
    try: yield lambda: used
    finally: contract.raw = original


@contextlib.contextmanager
def held_lock(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        require(stat.S_ISREG(os.fstat(fd).st_mode), 'ordinary existing lock required')
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB); yield
    finally: os.close(fd)


def command_evidence(W, C, recipe, ctx, binding, number):
    record = W.command_record(recipe, ctx, number, binding)
    config = C.document(recipe['config']); command=record['requested_arguments'][0]
    kind='page' if command in {'paths','packets','metadata-conflicts','issues'} else 'aggregate' if command in {'report','families'} else 'document'
    require(record['stdout_cap'] == config['stdout_caps_bytes'][kind] and record['stderr_cap'] == config['stderr_cap_bytes'], 'native stream limits differ')
    directory = Path(recipe['run'])/'commands'/f'{number:09d}'
    started = C.document(C.reference(directory/'started.json'))
    fields = {'sequence','key','requested_arguments','argv','capture_path','started_unix','source_binding'}
    require(set(started) == fields and all(record.get(k) == v for k,v in started.items()), 'native started identity')
    process = C.document(C.reference(directory/'process.json'))
    require(process.get('argv') == record['argv'] and type(process.get('pid')) is int and process['pid'] > 1
            and process.get('process_group') == process['pid']
            and record['started_unix'] <= process['started_unix'] <= record['finished_unix'], 'native process identity')
    for name in ['stdout','stderr']:
        desc = record[name]; path = C.absolute(str(Path(recipe['run'])/desc['path']))
        require(path == directory/name, 'native stream namespace')
        meta = path.lstat()
        require(type(desc['bytes']) is int and 0 <= desc['bytes'] <= record[name+'_cap']
                and stat.S_ISREG(meta.st_mode) and meta.st_size == desc['bytes']
                and isinstance(desc['sha256'], str) and len(desc['sha256']) == 64, 'native stream extent')
    return record


def audit(W, C, request):
    """Read only; caller independently owns/persists the review result."""
    start = time.monotonic()
    with metadata_budget(C) as used:
        recipe = C.document(request['recipe']); result = C.document(request['result'])
        require('packet_selection' in recipe and recipe['phase'] == 'packets', 'selected PACKETS only')
        unsigned={k:v for k,v in recipe.items() if k != 'grant'}
        require(C.document(recipe['grant']) == {'status':'EXECUTION_GRANTED','recipe_body_sha256':C.sha(C.encoded(unsigned)),
                'scope':'packets','attempt_id':recipe['attempt_id']}, 'selected execution grant differs')
        attempt = Path(recipe['control'])/'attempts'/recipe['attempt_id']; run = C.absolute(recipe['run'])
        require(request['result']['path'] == str(attempt/'result.json')
                and request['recipe']['path'] == str(attempt/'recipe.json'), 'owner recipe/result namespace')
        wait = C.document(request['terminal_wait']); association = C.document(request['terminal_association'])
        require(type(wait.get('exit_code')) is int and wait['exit_code'] == 0 and not wait.get('session_id'), 'outer terminal not reaped')
        require(association.get('status') == 'PASS' and association.get('terminal_wait') == request['terminal_wait']
                and association.get('recipe') == request['recipe'] and association.get('result') == request['result']
                and isinstance(association.get('tool_session_id'), str) and association['tool_session_id']
                and isinstance(association.get('reviewer'), str) and association['reviewer'].strip(), 'outer terminal association')
        require(result.get('root_reaped') is True and result.get('ownership_status') == 'observed_owned_processes_reaped'
                and result.get('cleanup') is None and result.get('new_command_failures') == []
                and not any(k in result for k in ['failure','pipe_failure','funding_receipt_error']), 'failed/unowned selected result')
        require(set(result.get('logs', {})) == {'stdout','stderr'}, 'owner log roster')
        for log in result['logs'].values():
            require(log.get('complete') is True and log.get('error') is None and log.get('truncated') is False
                    and type(log.get('observed_bytes')) is int and log['observed_bytes'] == log.get('retained_bytes')
                    and 0 <= log['retained_bytes'] <= W.LOG_CAP, 'owner log incomplete')
            require(len(C.checked(log['reference'], W.LOG_CAP)) == log['retained_bytes'], 'owner log bytes differ')
        require(result.get('status') in {'paused_at_command_boundary','review_returned_not_acceptance'}
                and type(result.get('exit_code')) is int, 'unknown selected status')
        profile = W.selected_native_profile(recipe)
        ctx = W.native_context(recipe, C.context(recipe), C.document(recipe['binding']), profile)
        binding = C.document(recipe['binding'])
        with held_lock(C.absolute(str(Path(recipe['control'])/'owner.lock'))), held_lock(C.absolute(str(run/'runner.lock'))):
            require(W.read(Path(recipe['control'])/'current.json') == {'attempt_id':recipe['attempt_id']}, 'current owner changed')
            require(C.reference(run/'journal.json') == result['journal']
                    and W.read(run/'journal.json') == {'next_command':result['next_command']}, 'current journal changed')
            require(not os.path.lexists(run/'commands'/f"{result['next_command']:09d}"), 'next command reserved')
            W.validate_selected_previous_history(recipe)
            W.validate_execution_profile(recipe, attempt, result); W.validate_native_execution(recipe, attempt, result)
            first = C.integer(recipe['expected_next_command'], 1); end = C.integer(result['next_command'], first)
            require(end-first <= W.MAX_COMMANDS, 'command count cap')
            digest = hashlib.sha256(); record_bytes = 0
            for number in range(first, end):
                require(time.monotonic()-start < 120, 'audit deadline')
                path = run/'commands'/f'{number:09d}'/'result.json'
                record_bytes += path.lstat().st_size; require(record_bytes <= W.MAX_METADATA, 'result metadata cap')
                record = command_evidence(W,C,recipe,ctx,binding,number)
                require(type(record.get('exit_code')) is int and record['exit_code'] == 0
                        and record.get('failure') is None and record.get('log_errors') == [], 'failed native command')
                digest.update(C.encoded({'sequence':number,'result':C.reference(path)}))
            phase_path=C.absolute(result['phase']['path'])
            require(phase_path.parent == run/'reports' and phase_path.name.startswith('phase-'), 'phase namespace')
            process=C.document(C.reference(attempt/'process.json'))
            expected_argv=[recipe['code']['python']['path'],'-I','-B',recipe['code']['controller']['path'],
                           '--child',request['recipe']['path'],request['recipe']['sha256']]
            require(process.get('argv') == expected_argv, 'owner child argv differs')
            phase = C.document(result['phase'])
            require(phase.get('binding') == binding and phase.get('phase') == 'packets'
                    and phase.get('input') == recipe['input']['path'], 'phase provenance differs')
            if result['status'] == 'paused_at_command_boundary':
                require(result['exit_code'] == 1 and phase.get('status') == 'paused'
                        and result.get('pause') == C.reference(run/'pause-request'), 'pause classification')
                require(C.document(result['pause']).get('owner'), 'unknown pause owner')
            else:
                require(result['exit_code'] == 0 and phase.get('status') == 'review_artifact_returned_not_acceptance'
                        and result.get('output') == C.reference(ctx['output']) and phase.get('output') == ctx['output'], 'terminal output classification')
                if os.path.lexists(run/'pause-request'):
                    require(result.get('pause') == C.reference(run/'pause-request') and C.document(result['pause']).get('owner'), 'terminal pause differs')
                else: require(result.get('pause') is None, 'terminal pause disappeared')
                output = C.document(result['output']); C.validate_output(output,ctx)
                validate_output_commands(W,C,recipe,ctx,binding,output,end)
        return {'status':'PASS','scope':'selected PACKETS metadata/provenance; not S9 acceptance or grant',
                'recipe':request['recipe'],'result':request['result'],'binding':recipe['binding'],
                'output':result.get('output'),'phase':result['phase'],'previous':recipe['previous'],
                'packet_selection':recipe['packet_selection'],'native_execution_profile':recipe['native_execution_profile'],
                **{k:result[k] for k in ['execution_profile','execution_profile_consumed','native_execution','native_execution_consumed']},
                'root_terminal_wait':request['terminal_wait'],'terminal_association':request['terminal_association'],
                'first_command':first,'next_command':end,'command_chain_sha256':digest.hexdigest(),
                'metadata_bytes_read':used(),'elapsed_seconds':time.monotonic()-start,
                'limitations':['PAGE/body digests are recorded native claims, not independently rehashed bodies.',
                               'Unselected backup external packets remain unassessed; all prerequisite evidence is retained.']}


def validate_output_commands(W,C,recipe,ctx,binding,output,end):
    """Exact selected report/page provenance, including omitted terminal empties."""
    run = Path(recipe['run'])
    def value(record):
        require(record['exit_code'] == 0 and record.get('failure') is None and record.get('log_errors') == [], 'failed output command')
        return C.document({'path':str(run/record['stdout']['path']),'sha256':record['stdout']['sha256']})
    def keyed(key):
        step = W.read(run/'steps'/(C.sha(C.encoded(key))+'.json'))
        n = C.integer(step['sequence'],1); require(n < end, 'output command after terminal')
        record = command_evidence(W,C,recipe,ctx,binding,n)
        require(record['key'] == key, 'output command key differs')
        return record
    for row in output['outcomes']:
        # A native zero ending is required; another report is not proof that the
        # preceding inspection completed. Read only these small command outputs.
        for index in range(C.document(recipe['config'])['maximum_calls_per_revision']):
            checked = value(keyed([ctx['tag'],row['revision'],'check',index]))
            if C.integer(checked['checked']) == 0: break
        else: raise ValueError('missing selected check zero ending')
        rev = row['revision']
        for name, summary in row['pages'].items():
            pages = summary['pages']
            require(isinstance(pages,list) and len(pages) <= 20000, 'page roster bound')
            for index in range(len(pages)+1):
                key = [ctx['tag'],rev,name,index]
                step = W.read(run/'steps'/(C.sha(C.encoded(key))+'.json')); n = C.integer(step['sequence'],1)
                require(n < end and (index == len(pages) or n == pages[index]), 'page command roster differs')
                record = command_evidence(W,C,recipe,ctx,binding,n)
                require(record['key'] == key and record['exit_code'] == 0 and record['failure'] is None, 'page command failed')
                if index == len(pages): require(record['stdout']['bytes'] == 3, 'terminal page extent differs')
                elif index == 0: require(record['requested_arguments'][4] == '0', 'first page cursor differs')
            C.integer(summary['rows']); C.integer(summary['last_sequence'])
            require(sum(C.integer(v) for v in summary['counts'].values()) == summary['rows'], 'page state count reconciliation')
        key = [ctx['tag'],rev,'report']; step = W.read(run/'steps'/(C.sha(C.encoded(key))+'.json'))
        n = C.integer(step['sequence'],1); require(n < end, 'report after terminal')
        record = command_evidence(W,C,recipe,ctx,binding,n)
        require(value(record) == row['report'], 'reported member differs')
        counts = row['report']['counts']; states = row['pages']['paths']['counts']
        require({k[6:]:C.integer(v) for k,v in counts.items() if k.startswith('paths_') and v} ==
                {k:C.integer(v) for k,v in states.items() if v}, 'report/path states differ')
        require(not states.get('pending',0) and not states.get('available_packets_uninspected',0), 'selected paths remain uninspected')
    require(value(keyed([ctx['tag'],'families'])) == output['families'], 'family evidence differs')
    baseline_record=W.read(run/'commands'/f"{ctx['input']['ending_inventory_command']:09d}"/'result.json')
    require(baseline_record['source_binding'] == binding and baseline_record['requested_arguments'] ==
            ['discover',C.document(recipe['config'])['catalog_root']], 'FULL baseline command differs')
    baseline=value(baseline_record)
    def inventory(v):
        require(v.get('complete') is True, 'incomplete inventory')
        rows=v['candidates']; keys={C.sha(C.encoded(r['path'])):r for r in rows}
        require(len(keys)==len(rows), 'duplicate inventory candidate')
        return keys
    expected=inventory(baseline)
    for field, suffix in [('starting_inventory_command','discover-admission'),('ending_inventory_command','discover-end')]:
        n=C.integer(output[field],1); require(n < end,'inventory after terminal')
        record=command_evidence(W,C,recipe,ctx,binding,n)
        require(record['key'][2] == suffix,'inventory command kind differs')
        require(inventory(value(record)) == expected, 'selected inventory differs from FULL')


def main():
    # The caller pins this file as well as the controller/contract; never import a
    # mutable worktree helper through ambient sys.path.
    parser=argparse.ArgumentParser(description=__doc__); parser.add_argument('request'); parser.add_argument('sha256')
    args=parser.parse_args(); path=Path(args.request)
    with path.open('rb') as stream: data=stream.read(16*1024**2+1)
    require(len(data)<=16*1024**2 and hashlib.sha256(data).hexdigest()==args.sha256,'audit request changed')
    request=json.loads(data)
    import types
    def load(ref):
        with Path(ref['path']).open('rb') as stream: source=stream.read(16*1024**2+1)
        require(len(source)<=16*1024**2 and hashlib.sha256(source).hexdigest()==ref['sha256'],'auditor helper changed')
        value=types.ModuleType(ref['path']); value.__file__=ref['path']; exec(compile(source,ref['path'],'exec'),value.__dict__); return value
    W=load(request['controller']); C=load(request['contract']); W.C=C
    require(C.reference(__file__)==request['auditor'],'auditor source changed')
    recipe=C.document(request['recipe'])
    require(recipe['code']['controller']==request['controller'] and recipe['code']['contract']==request['contract'],'audit source association')
    value=audit(W,C,request); value['author']=request['author']
    destination=C.absolute(request['output']); encoded=C.encoded(value)
    require(len(encoded)<=4*1024**2,'audit output cap')
    with destination.open('xb') as stream: stream.write(encoded); stream.flush(); os.fsync(stream.fileno())
    print(json.dumps({'status':'PASS','review':C.reference(destination)}))


if __name__ == '__main__': main()
