"""Serial S8 child ownership and resource guard, source-only pending final binding.

Commands are derived from a reviewed binding, never interpreted by a shell. The
binding must declare no unresolved execution gates; current planning checkpoints
therefore cannot launch a campaign. No retries and no failed-output cleanup.
"""
from __future__ import annotations
import argparse
from contextlib import contextmanager
import json
import os
import math
import re
import signal
import subprocess
import sys
import time
import threading
from pathlib import Path
import psutil
import edit_admission
import edit_cleanup
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


def owned_process(key):
    """Return the same identity-bound Process that will receive a signal."""
    try:
        process=psutil.Process(key[0])
        if process.create_time()!=key[1]:
            return None
        return process
    except psutil.NoSuchProcess:
        return None


@contextmanager
def cleanup_signals():
    # A second coordinator interrupt must not abort its bounded reap sequence.
    prior={}
    if threading.current_thread() is threading.main_thread():
        for signum in (signal.SIGINT,signal.SIGTERM):
            prior[signum]=signal.signal(signum,signal.SIG_IGN)
    try:
        yield
    finally:
        for signum,handler in prior.items():
            signal.signal(signum,handler)


def terminate_owned(child,known):
    errors=[]
    def failed(stage,exc):
        value=stage+': '+type(exc).__name__+': '+str(exc)[:512]
        if value not in errors and len(errors)<64:
            errors.append(value)

    with cleanup_signals():
        # Popen owns the unreaped child on POSIX and a process handle on Windows.
        # Its signal methods avoid a reaped/reused root PID. Never substitute a
        # raw os.kill(pid) or a process-group ID recovered from telemetry here.
        try:
            child.terminate()
        except ProcessLookupError:
            pass
        except Exception as exc:
            failed('root terminate',exc)
        for key in known:
            try:
                process=owned_process(key)
                if process is not None:
                    process.terminate()  # psutil rechecks this object's identity.
            except psutil.NoSuchProcess:
                pass
            except Exception as exc:
                failed('known terminate',exc)
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            pass
        except Exception as exc:
            failed('root wait',exc)
        # Always reach the Popen kill/wait fallback, including failed discovery
        # and AccessDenied while inspecting any descendant. No telemetry call is
        # a prerequisite for reaping the root we launched.
        try:
            child.kill()
        except ProcessLookupError:
            pass
        except Exception as exc:
            failed('root kill',exc)
        for key in known:
            try:
                process=owned_process(key)
                if process is not None:
                    process.kill()
            except psutil.NoSuchProcess:
                pass
            except Exception as exc:
                failed('known kill',exc)
        try:
            child.wait(timeout=5)
        except Exception as exc:
            failed('root final wait',exc)
        remaining=[]
        deadline=time.monotonic()+3
        while True:
            remaining=[]
            for key in known:
                try:
                    if same_alive(key):
                        remaining.append(key)
                except Exception as exc:
                    failed('known final inspection',exc)
                    remaining.append(key)
            if not remaining or errors or time.monotonic()>=deadline:
                break
            time.sleep(.05)
        return dict(known_absent=not remaining and not errors and child.returncode is not None,
                    scope='Popen root and individually discovered identities; not undiscovered descendants',
                    remaining=remaining,errors=errors,root_reaped=child.returncode is not None,
                    root_returncode=child.returncode)


class BoundedCapture:
    """Retain a bounded prefix; drain overflow so cleanup cannot deadlock on IO."""
    def __init__(self,pipe,path,limit):
        self.pipe=pipe
        self.path=Path(path)
        self.limit=limit
        self.observed=0
        self.retained=0
        self.eof=False
        self.errors=[]
        self.failed=threading.Event()
        self.lock=threading.Lock()
        self.thread=threading.Thread(target=self.collect,name='edit-evidence-'+self.path.name,daemon=True)
        self.thread.start()

    def fault(self,exc):
        with self.lock:
            if len(self.errors)<8:
                self.errors.append(type(exc).__name__+': '+str(exc)[:512])
        self.failed.set()

    def collect(self):
        output=None
        try:
            try:
                output=self.path.open('xb',buffering=0)
            except Exception as exc:
                self.fault(exc)
            while True:
                part=self.pipe.read(65536)
                if not part:
                    with self.lock:
                        self.eof=True
                    break
                with self.lock:
                    self.observed+=len(part)
                    capacity=max(0,self.limit-self.retained)
                    if self.observed>self.limit:
                        self.failed.set()
                if output is not None and capacity:
                    try:
                        view=memoryview(part)[:capacity]
                        while view:
                            count=output.write(view)
                            if not count:
                                raise OSError('evidence write made no progress')
                            with self.lock:
                                self.retained+=count
                            view=view[count:]
                    except Exception as exc:
                        self.fault(exc)
                        broken=output
                        output=None
                        try:
                            broken.close()
                        except Exception as close_error:
                            self.fault(close_error)
        except Exception as exc:
            self.fault(exc)
        finally:
            if output is not None:
                try:
                    output.flush()
                    os.fsync(output.fileno())
                except Exception as exc:
                    self.fault(exc)
                finally:
                    try:
                        output.close()
                    except Exception as exc:
                        self.fault(exc)
            try:
                self.pipe.close()
            except Exception as exc:
                self.fault(exc)

    def finish(self):
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            self.fault(RuntimeError('pipe EOF not established after owned process cleanup'))
        with self.lock:
            return dict(limit_bytes=self.limit,observed_bytes=self.observed,
                        retained_bytes=self.retained,truncated=self.observed>self.retained,
                        eof=self.eof,reader_joined=not self.thread.is_alive(),errors=list(self.errors))


def supervision_settings(value=None):
    defaults=dict(max_active=MAX_ACTIVE,max_seen=MAX_SEEN,max_telemetry_bytes=MAX_TELEMETRY,
                  max_sample_bytes=8192,max_identity_bytes=256*1024,max_identity_event_bytes=512)
    if value is not None:
        if set(value)!=set(defaults):
            raise ValueError('complete explicit supervision settings required')
        defaults=dict(value)
    maxima=dict(max_active=8,max_seen=131072,max_telemetry_bytes=8*1024**3,
                max_sample_bytes=8192,max_identity_bytes=128*MIB,max_identity_event_bytes=512)
    for key,value in defaults.items():
        if type(value) is not int or not 0<value<=maxima[key]:
            raise ValueError('supervision admission: '+key)
    return defaults


def descendant_identity(process,root_key):
    """Validate a fresh ancestry chain, not just a PID returned by enumeration."""
    enumerated=(process.pid,process.create_time())
    current=owned_process(enumerated)
    if current is None:
        return None
    chain=[]
    for _ in range(64):
        key=(current.pid,current.create_time())
        current=owned_process(key)
        if current is None:return None
        if key==root_key:
            # Bind both sides of every observed edge again. Reparenting or PID
            # reuse is uncertainty, never permission to claim the new process.
            if not same_alive(root_key):
                return None
            for child_key,parent_key in chain:
                child=owned_process(child_key)
                parent=owned_process(parent_key)
                if child is None or parent is None:
                    return None
                if child.ppid()!=parent_key[0]:
                    raise RuntimeError('owned ancestry changed during admission')
            return enumerated
        parent=current.parent()
        if parent is None:
            raise RuntimeError('discovered process lacks current root ancestry')
        parent_key=identity(parent)
        chain.append((key,parent_key))
        current=parent
    raise RuntimeError('owned ancestry depth admission')


class ActiveIdentities:
    """Only unresolved identities are scanned. Terminal proofs remain on disk."""
    def __init__(self,stream,settings):
        self.stream=stream
        self.settings=settings
        self.active={}
        self.discovered=0
        self.retired=0
        self.bytes=0

    def event(self,key,kind,reason=None):
        value=dict(kind=kind,pid=key[0],create_time=key[1],at=anchor())
        if reason is not None:value['reason']=reason
        line=(json.dumps(value,allow_nan=False,separators=(',',':'))+'\n').encode()
        if len(line)>self.settings['max_identity_event_bytes'] or self.bytes+len(line)>self.settings['max_identity_bytes']:
            raise RuntimeError('identity evidence byte admission exceeded')
        if self.stream.write(line)!=len(line):
            raise OSError('identity evidence write made no progress')
        self.stream.flush()
        self.bytes+=len(line)

    def add(self,key):
        if key in self.active:return
        # Keep the just-discovered identity for cleanup even if admission or its
        # evidence write fails. At most one overflow identity is added.
        self.active[key]=True
        self.discovered+=1
        if self.discovered>self.settings['max_seen'] or len(self.active)>self.settings['max_active']:
            raise RuntimeError('owned process-count admission exceeded')
        self.event(key,'discovered')

    def retire(self,key,reason):
        self.event(key,'retired',reason)
        del self.active[key]
        self.retired+=1

    def sample(self):
        records=[]
        for key in tuple(self.active):
            try:
                process=owned_process(key)
                if process is None:
                    self.retire(key,'absent_or_replaced')
                    continue
                status=process.status()
                if status==psutil.STATUS_ZOMBIE:
                    self.retire(key,'zombie')
                    continue
                memory=process.memory_info()
                cpu=process.cpu_times()
                # ASCII escaping makes the encoded name bound deterministic.
                records.append(dict(pid=key[0],create_time=key[1],name=process.name()[:64],status=status,
                                    rss=memory.rss,cpu_seconds=cpu.user+cpu.system))
            except psutil.NoSuchProcess:
                self.retire(key,'absent_during_sample')
        return records


def invoke(command, folder, limits, disk_root, *, supervision=None):
    settings=supervision_settings(supervision)
    timeout=limits.get('deadline_seconds')
    if type(timeout) not in (int,float) or not math.isfinite(timeout) or not 0<timeout<=86400:
        raise ValueError('positive finite at-most-24-hour owner deadline required')
    folder=Path(folder)
    folder.mkdir()
    started=anchor()
    deadline=started["monotonic_ns"]/1e9+limits["deadline_seconds"]
    exclusive(folder/'start.json',dict(command=command,limits=limits,started=started,
                                      stdout='stdout.log',stderr='stderr.log',supervision=settings))
    child=None
    known={}
    tracker=None
    captures={}
    samples=0
    errors=[]
    peak=0
    ownership=None
    telemetry_bytes=0
    try:
        with (folder/'processes.jsonl').open('xb') as telemetry, (folder/'identities.jsonl').open('xb') as identities:
            tracker=ActiveIdentities(identities,settings)
            known=tracker.active
            child=subprocess.Popen(command,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                                   bufsize=0,start_new_session=os.name=='posix',
                                   creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=='nt' else 0)
            for name,pipe in (('stdout.log',child.stdout),('stderr.log',child.stderr)):
                captures[name]=BoundedCapture(pipe,folder/name,MAX_STDIO)
            root=psutil.Process(child.pid)
            root_key=identity(root)
            tracker.add(root_key)
            exclusive(folder/'spawn.json',dict(pid=child.pid,create_time=root_key[1],at=anchor()))
            while True:
                at=anchor()
                # Retire completed work before admitting a new generation. Old
                # numeric PIDs therefore never bloat or block active sampling.
                tracker.sample()
                try:
                    if same_alive(root_key):
                        for process in psutil.Process(child.pid).children(recursive=True):
                            key=descendant_identity(process,root_key)
                            if key is not None:
                                tracker.add(key)
                except psutil.NoSuchProcess:
                    pass
                records=tracker.sample()
                total=sum(p['rss'] for p in records)
                peak=max(peak,total)
                free=psutil.disk_usage(disk_root).free
                line=(json.dumps(dict(at=at,processes=records,total_rss=total,free_bytes=free),allow_nan=False)+'\n').encode()
                if len(line)>settings['max_sample_bytes'] or telemetry_bytes+len(line)>settings['max_telemetry_bytes']:
                    raise RuntimeError('process telemetry byte cap exceeded')
                telemetry.write(line)
                telemetry.flush()
                telemetry_bytes+=len(line)
                samples+=1
                if len(records)>settings['max_active']:
                    raise RuntimeError('owned process-count admission exceeded')
                if total>limits['group_rss_bytes'] or any(p['rss']>limits['process_rss_bytes'] for p in records):
                    raise RuntimeError('sampled RSS admission exceeded')
                if free<limits['free_reserve_bytes']:
                    raise RuntimeError('live filesystem free-space reserve exhausted')
                if any(capture.failed.is_set() for capture in captures.values()):
                    raise RuntimeError('bounded stdout/stderr capture failed or overflowed')
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
            telemetry.flush()
            os.fsync(telemetry.fileno())
            identities.flush()
            os.fsync(identities.fileno())
    except BaseException as exc:
        errors.append(f'{type(exc).__name__}: {exc}')
    finally:
        # Evidence writes, telemetry and capture inspection cannot gate cleanup.
        if child is not None:
            ownership=terminate_owned(child,known)
        capture_proofs={}
        for name,capture in captures.items():
            try:
                proof=capture.finish()
                capture_proofs[name]=proof
                if proof['truncated'] or proof['errors'] or not proof['eof'] or not proof['reader_joined']:
                    errors.append('incomplete/overflowed capture: '+name)
            except Exception as exc:
                errors.append('capture finalization: '+type(exc).__name__+': '+str(exc))
        if child is not None:
            # A capture constructor may fail before taking ownership of a pipe.
            for name,pipe in (('stdout.log',child.stdout),('stderr.log',child.stderr)):
                if name not in captures and pipe is not None:
                    pipe.close()
    artifacts={}
    for name,limit in (('stdout.log',MAX_STDIO),('stderr.log',MAX_STDIO),('processes.jsonl',settings['max_telemetry_bytes']),('identities.jsonl',settings['max_identity_bytes'])):
        path=folder/name
        try:
            if path.exists():
                if name in ('processes.jsonl','identities.jsonl'):
                    # Flush already-written failure telemetry too; success-only
                    # fsync above does not cover interrupted sampling.
                    with path.open('r+b') as retained:
                        os.fsync(retained.fileno())
                artifacts[name]=dict(bytes=path.stat().st_size,sha256=digest(path,'sha256',limit))
        except Exception as exc:
            # Preserve the failure receipt even if retained evidence cannot be
            # hashed (IO fault, external mutation, or incomplete pipe capture).
            artifacts[name]=dict(error=type(exc).__name__+': '+str(exc))
            errors.append('artifact verification failed: '+name)
    result=dict(complete=not errors and ownership is not None and ownership['known_absent'],
                started=started,finished=anchor(),error='; '.join(errors) if errors else None,
                ownership=ownership,captures=capture_proofs,sampled_peak_group_rss=peak,samples=samples,
                supervision=settings,identity_counts=None if tracker is None else dict(
                    discovered=tracker.discovered,retired=tracker.retired,cleanup_candidates=list(tracker.active)),
                caveat='0.1 second sampled RSS/deadline guards are not hard allocation enforcement; streams retain bounded prefixes',
                **artifacts)
    exclusive(folder/'result.json',result)
    if not result['complete']:
        raise RuntimeError('child failed; retained at '+str(folder))
    return result


def validate_binding(binding):
    edit_admission.validate_execution(binding)
    if binding.get('version')!=2 or binding.get('pending_execution_gates')!=[]:
        raise ValueError('final reviewed protocol and resolved execution gates required')
    if binding.get('automatic_retries')!=0 or not binding.get('actions'):
        raise ValueError('explicit serial action set required')
    if len(binding['actions'])>2000:
        raise ValueError('campaign child bound')
    for key in ('python','probe','worker','verifier','fixture_generator'):
        item=binding[key]
        if not Path(item['path']).is_absolute() or digest(Path(item['path']).resolve(strict=True) if key=='python' else item['path'],'sha256',512*MIB)!=item['sha256']:
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
        if digest(Path(item['path']).resolve(strict=True) if key=='python' else item['path'],'sha256',512*MIB)!=item['sha256']:
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
    exclusive(root/'host-identity.json',host_identity(root,[a['path'] for a in binding.get('sources',[])]))
    results=[]
    error=None
    try:
        with HostObservation(root,**binding['outer_owner']['host_logs']) as host:
            for action in binding['actions']:
                if host.error is not None:raise RuntimeError('bounded host evidence failed: '+host.error)
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
                    command=action['command']
                elif kind=='generate':
                    command=[binding['python']['path'],binding['fixture_generator']['path'],'--admitted',
                             '--fixture',action['fixture'],'--output',str(root/(action['id']+'.tiff')),
                             '--receipt',str(root/(action['id']+'-fixture.json'))]
                else:
                    raise ValueError('unknown action')
                limits=dict(deadline_seconds=action['deadline_seconds'],
                            process_rss_bytes=action['process_rss_bytes'],group_rss_bytes=action['group_rss_bytes'],
                            free_reserve_bytes=binding['free_reserve_bytes'])
                if command!=action['command']:
                    raise ValueError('actual command differs from frozen argv')
                observed=invoke(command,folder,limits,root)
                if host.error is not None:raise RuntimeError('bounded host evidence failed: '+host.error)
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
                if kind=='verify':
                    record=next(item for item in binding['case_records'] if item['id']==action['id'][7:])
                    if record['cleanup_path'] is not None:
                        edit_cleanup.cleanup_export(root,record)
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
