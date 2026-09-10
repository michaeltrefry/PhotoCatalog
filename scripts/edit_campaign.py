"""Serial S8 child ownership and resource guard, source-only pending final binding.

Commands are derived from a reviewed binding, never interpreted by a shell. The
binding must declare no unresolved execution gates; current planning checkpoints
therefore cannot launch a campaign. No retries and no failed-output cleanup.
"""
from __future__ import annotations
import argparse
import json
import os
import math
import re
import signal
import subprocess
import sys
import time
from pathlib import Path
import psutil
from preview_host import HostObservation, host_identity
from edit_verify import digest, read_json

MIB=1024**2
MAX_STDIO=4*MIB
MAX_TELEMETRY=32*MIB
MAX_ACTIVE=4
MAX_SEEN=256


def anchor():
    return dict(unix_ns=time.time_ns(),monotonic_ns=time.monotonic_ns())


def exclusive(path,value):
    with Path(path).open('x') as stream:
        json.dump(value,stream,indent=2,allow_nan=False)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def identity(process):
    # New Process object per sample: never rely on process_iter cached create_time.
    fresh=psutil.Process(process.pid)
    return fresh.pid,fresh.create_time()


def same_alive(key):
    try:
        process=psutil.Process(key[0])
        return process.create_time()==key[1] and process.status()!=psutil.STATUS_ZOMBIE
    except psutil.NoSuchProcess:
        return False


def terminate_owned(child,known):
    errors=[]
    live=[]
    for key in known:
        try:
            if same_alive(key):
                live.append(psutil.Process(key[0]))
        except psutil.Error as exc:
            errors.append(type(exc).__name__)
    # Only the group created by this Popen, while one known identity still belongs
    # to that group. No PID guessed from a stale receipt is ever signaled.
    if os.name=='posix':
        grouped=[]
        for process in live:
            try:
                if os.getpgid(process.pid)==child.pid:
                    grouped.append(process)
                else:
                    errors.append('owned child escaped expected process group')
            except ProcessLookupError:
                pass
        if grouped:
            try:
                os.killpg(child.pid,signal.SIGTERM)
            except ProcessLookupError:
                pass
    else:
        for process in reversed(live):
            try:
                process.terminate()
            except psutil.NoSuchProcess:
                pass
    deadline=time.monotonic()+3
    while time.monotonic()<deadline and any(same_alive(k) for k in known):
        time.sleep(.05)
    for key in known:
        try:
            if same_alive(key):
                psutil.Process(key[0]).kill()
        except psutil.Error as exc:
            errors.append(type(exc).__name__)
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        errors.append('root was not reaped')
    remaining=[]
    for key in known:
        try:
            if same_alive(key):
                remaining.append(key)
        except psutil.Error as exc:
            errors.append(type(exc).__name__)
    return dict(known_absent=not remaining and not errors,remaining=remaining,errors=errors,
                root_returncode=child.returncode)


def invoke(command, folder, limits, disk_root):
    folder=Path(folder)
    folder.mkdir()
    started=anchor()
    exclusive(folder/'start.json',dict(command=command,limits=limits,started=started,
                                      stdout='stdout.log',stderr='stderr.log'))
    child=None
    known={}
    samples=0
    error=None
    peak=0
    ownership=None
    try:
        with (folder/'stdout.log').open('xb') as stdout, (folder/'stderr.log').open('xb') as stderr, (folder/'processes.jsonl').open('x') as telemetry:
            child=subprocess.Popen(command,stdin=subprocess.DEVNULL,stdout=stdout,stderr=stderr,
                                   start_new_session=os.name=='posix',
                                   creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=='nt' else 0)
            root=psutil.Process(child.pid)
            root_key=identity(root)
            known[root_key]=True
            exclusive(folder/'spawn.json',dict(pid=child.pid,create_time=root_key[1],at=anchor()))
            deadline=time.monotonic()+limits['deadline_seconds']
            while True:
                at=anchor()
                records=[]
                # Root can exit before a poll; previously identified descendants
                # remain checked until absent. Unknown ownership is a failure.
                try:
                    if same_alive(root_key):
                        found=[root]+psutil.Process(child.pid).children(recursive=True)
                        for process in found:
                            key=identity(process)
                            known[key]=True
                except psutil.NoSuchProcess:
                    pass
                for key in known:
                    try:
                        process=psutil.Process(key[0])
                        if identity(process)!=key:
                            continue
                        status=process.status()
                        if status==psutil.STATUS_ZOMBIE:
                            continue
                        memory=process.memory_info()
                        cpu=process.cpu_times()
                        records.append(dict(pid=key[0],create_time=key[1],name=process.name(),status=status,
                                            rss=memory.rss,cpu_seconds=cpu.user+cpu.system))
                    except psutil.NoSuchProcess:
                        pass
                total=sum(p['rss'] for p in records)
                peak=max(peak,total)
                free=psutil.disk_usage(disk_root).free
                telemetry.write(json.dumps(dict(at=at,processes=records,total_rss=total,free_bytes=free))+'\n')
                telemetry.flush()
                samples+=1
                if len(known)>MAX_SEEN or len(records)>MAX_ACTIVE:
                    raise RuntimeError('owned process-count admission exceeded')
                if total>limits['group_rss_bytes'] or any(p['rss']>limits['process_rss_bytes'] for p in records):
                    raise RuntimeError('sampled RSS admission exceeded')
                if free<limits['free_reserve_bytes']:
                    raise RuntimeError('live filesystem free-space reserve exhausted')
                if any((folder/name).stat().st_size>MAX_STDIO for name in ('stdout.log','stderr.log')) or telemetry.tell()>MAX_TELEMETRY:
                    raise RuntimeError('evidence stream byte cap exceeded')
                code=child.poll()
                if code is not None:
                    if any(key!=root_key and same_alive(key) for key in known):
                        raise RuntimeError('root exited with a known unreaped descendant')
                    if code:
                        raise RuntimeError('child returned nonzero status '+str(code))
                    break
                if time.monotonic()>=deadline:
                    raise RuntimeError('sampled child deadline exceeded')
                time.sleep(.1)
        ownership=terminate_owned(child,known)
    except BaseException as exc:
        error=f'{type(exc).__name__}: {exc}'
        if child is not None:
            ownership=terminate_owned(child,known)
    result=dict(complete=error is None and ownership is not None and ownership['known_absent'],
                started=started,finished=anchor(),error=error,ownership=ownership,
                sampled_peak_group_rss=peak,samples=samples,
                caveat='0.1 second sampled guards are not hard per-allocation RSS enforcement')
    for name in ('stdout.log','stderr.log','processes.jsonl'):
        path=folder/name
        if path.exists():
            result[name]=dict(bytes=path.stat().st_size,sha256=digest(path,'sha256',MAX_TELEMETRY+MIB))
    exclusive(folder/'result.json',result)
    if not result['complete']:
        raise RuntimeError('child failed; retained at '+str(folder))
    return result


def validate_binding(binding):
    if binding.get('version')!=2 or binding.get('pending_execution_gates')!=[]:
        raise ValueError('final reviewed protocol and resolved execution gates required')
    if binding.get('automatic_retries')!=0 or not binding.get('actions'):
        raise ValueError('explicit serial action set required')
    if len(binding['actions'])>2000:
        raise ValueError('campaign child bound')
    for key in ('python','probe','worker','verifier','fixture_generator'):
        item=binding[key]
        if not Path(item['path']).is_absolute() or digest(item['path'],'sha256',512*MIB)!=item['sha256']:
            raise ValueError('executable/script identity: '+key)
    if binding['minimum_free_bytes']<binding['retained_bound_bytes']+binding['active_bound_bytes']+binding['copies_bound_bytes']+binding['free_reserve_bytes']:
        raise ValueError('minimum free space does not fund bounded peak and reserve')
    for action in binding['actions']:
        if action.get('kind') not in ('probe','verify','generate'):
            raise ValueError('unsupported action kind')
        for name in ('deadline_seconds','process_rss_bytes','group_rss_bytes'):
            value=action.get(name)
            if type(value) not in (int,float) or not math.isfinite(value) or value<=0:
                raise ValueError('positive finite action admission required')
        if action['deadline_seconds']>3600 or action['process_rss_bytes']>action['group_rss_bytes']:
            raise ValueError('action deadline/process memory bound')
    for name in ('source_archive','build_reference','protocol'):
        item=binding[name]
        if digest(item['path'],'sha256',512*MIB)!=item['sha256']:
            raise ValueError('build/protocol evidence identity: '+name)
    if re.fullmatch('[0-9a-f]{40}',binding.get('source_commit','')) is None:
        raise ValueError('full source revision required')
    identifiers=[a['id'] for a in binding['actions']]
    if len(identifiers)!=len(set(identifiers)) or any(not isinstance(x,str) or re.fullmatch(r'[A-Za-z0-9_-]{1,120}',x) is None for x in identifiers):
        raise ValueError('exclusive action identities')


def execute(binding,root):
    validate_binding(binding)
    root=Path(root)
    root.mkdir()
    exclusive(root/'binding.json',binding)
    if psutil.disk_usage(root).free<binding['minimum_free_bytes']:
        raise ValueError('insufficient fully funded campaign free space')
    exclusive(root/'host-identity.json',host_identity(root,[a['source'] for a in binding.get('sources',[])]))
    results=[]
    error=None
    try:
        with HostObservation(root):
            for action in binding['actions']:
                folder=root/action['id']
                kind=action['kind']
                if kind=='probe':
                    request=action['request']
                    expected=root/(action['id']+'-output')
                    if Path(request['output'])!=expected or request['worker']!=binding['worker']['path']:
                        raise ValueError('request ownership/binary binding')
                    request_path=root/(action['id']+'-request.json')
                    exclusive(request_path,request)
                    command=[binding['probe']['path'],'--request',str(request_path)]
                elif kind=='verify':
                    target=root/action['probe_output']
                    if root.resolve() not in target.resolve().parents:
                        raise ValueError('verifier target outside campaign')
                    command=[binding['python']['path'],binding['verifier']['path'],'--root',str(target),
                             '--output',str(root/(action['id']+'-verification.json'))]
                elif kind=='generate':
                    command=[binding['python']['path'],binding['fixture_generator']['path'],'--admitted',
                             '--fixture',action['fixture'],'--output',str(root/(action['id']+'.tiff')),
                             '--receipt',str(root/(action['id']+'-fixture.json'))]
                else:
                    raise ValueError('unknown action')
                limits=dict(deadline_seconds=action['deadline_seconds'],
                            process_rss_bytes=action['process_rss_bytes'],group_rss_bytes=action['group_rss_bytes'],
                            free_reserve_bytes=binding['free_reserve_bytes'])
                observed=invoke(command,folder,limits,root)
                if kind=='probe':
                    proof=read_json(expected/'receipt.json')
                    if proof.get('probe_complete') is not True:
                        raise ValueError('zero exit without complete probe receipt')
                elif kind=='verify':
                    proof=read_json(root/(action['id']+'-verification.json'),16*MIB)
                    if proof.get('complete') is not True or proof.get('result',{}).get('verified') is not True:
                        raise ValueError('zero exit without complete independent verification')
                else:
                    proof=read_json(root/(action['id']+'-fixture.json'))
                    if proof.get('id')!=action['fixture'] or digest(root/(action['id']+'.tiff'),'sha256',2*1024**3)!=proof.get('sha256'):
                        raise ValueError('generated source receipt/bytes disagree')
                results.append(dict(id=action['id'],result=observed,proof=proof))
    except BaseException as exc:
        error=f'{type(exc).__name__}: {exc}'
    exclusive(root/'campaign.json',dict(version=2,complete=error is None,error=error,results=results,
                                       qualification_complete=False,acceptance='independent aggregate audit required'))
    if error:
        raise RuntimeError(error)


def main():
    p=argparse.ArgumentParser()
    p.add_argument('--binding',type=Path,required=True)
    p.add_argument('--binding-sha256',required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--admitted',action='store_true',required=True)
    args=p.parse_args()
    if digest(args.binding,'sha256',16*MIB)!=args.binding_sha256:
        raise ValueError('reviewed binding identity mismatch')
    def interrupted(signum, frame):
        raise InterruptedError('coordinator signal '+str(signum))
    signal.signal(signal.SIGINT,interrupted)
    signal.signal(signal.SIGTERM,interrupted)
    execute(read_json(args.binding,16*MIB),args.output)

if __name__=='__main__':
    main()
