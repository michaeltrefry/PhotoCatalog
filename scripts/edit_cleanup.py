"""Delete only verified disposable export fixtures, preserving sealed evidence.

This is not a product garbage collector. Exact published receipts authorize each
recovery directory; unknown prefix matches, symlinks and failed jobs are retained.
"""
import hashlib
import json
import os
from pathlib import Path
import stat
from edit_verify import digest,read_json,observations,sample_coverage,owned,native_path

MAX_ENTRIES=4096
STORAGE_FIELDS={'root','device','inode','parent','parent_device','parent_inode','reserve_bytes'}


def exclusive(path,value):
    with Path(path).open('x') as stream:
        json.dump(value,stream,indent=2,allow_nan=False);stream.write('\n')
        stream.flush();os.fsync(stream.fileno())


def stamp(path):
    value=Path(path).lstat()
    if not stat.S_ISREG(value.st_mode):raise ValueError('ordinary deletion target required')
    return dict(bytes=value.st_size,device=value.st_dev,inode=value.st_ino,
                mtime_ns=value.st_mtime_ns,ctime_ns=value.st_ctime_ns)


def tree(root,roots,max_file,max_total):
    pending=list(roots);files=[];directories=[];total=0
    while pending:
        if len(pending)+len(files)+len(directories)>MAX_ENTRIES:raise ValueError('cleanup entry bound')
        path=owned(root,pending.pop())
        info=path.lstat()
        if stat.S_ISDIR(info.st_mode):
            directories.append(str(path))
            with os.scandir(path) as entries:
                for entry in entries:
                    pending.append(Path(entry.path))
                    if len(pending)+len(files)+len(directories)>MAX_ENTRIES:raise ValueError('cleanup entry bound')
        elif stat.S_ISREG(info.st_mode):
            identity=stamp(path);total+=identity['bytes']
            if total>max_total:raise ValueError('cleanup logical-byte bound')
            sha=digest(path,'sha256',max_file)
            if stamp(path)!=identity:raise ValueError('cleanup file changed during hashing')
            files.append(dict(path=str(path),sha256=sha,**identity))
        else:raise ValueError('cleanup refuses special files')
    files.sort(key=lambda item:item['path'])
    directories.sort(key=lambda item:(-len(Path(item).parts),item))
    return files,directories


def retired(path):
    result=read_json(path,1024*1024)
    ownership=result.get('ownership',{})
    if (result.get('complete') is not True or result.get('error') is not None
        or ownership.get('known_absent') is not True or ownership.get('root_reaped') is not True
        or ownership.get('root_returncode')!=0 or ownership.get('remaining')!=[] or ownership.get('errors')!=[]):
        raise ValueError('cleanup requires complete successful process retirement')


def retired_storage(path):
    path=Path(path)
    retired(path)
    start=read_json(path.parent/'start.json',1024*1024)
    storage=start.get('limits',{}).get('storage')
    if (not isinstance(storage,dict) or set(storage)!= {'artifact','service'}
        or any(not isinstance(value,dict) or set(value)!=STORAGE_FIELDS
               or any(type(value[key]) is not int for key in (
                   'device','inode','parent_device','parent_inode','reserve_bytes'))
               or value['reserve_bytes']<0 for value in storage.values())):
        raise ValueError('cleanup requires exact supervisor storage admission')
    return storage


class StorageGuard:
    """Pin one admitted root and address POSIX removals through its held fd."""
    def __init__(self,name,descriptor):
        self.name=name
        self.expected=descriptor
        self.path=Path(descriptor['root'])
        self.parent=Path(descriptor['parent'])
        self.handles=[]
        self.fd=None
        self.close_handle=None

    def identity(self,path,prefix=''):
        value=path.lstat()
        if not stat.S_ISDIR(value.st_mode):
            raise ValueError(self.name+' storage path is not an ordinary directory')
        return {prefix+'device':value.st_dev,prefix+'inode':value.st_ino}

    def check(self):
        if (not self.path.is_absolute() or not self.parent.is_absolute()
            or self.path.parent!=self.parent
            or self.path.resolve(strict=True)!=self.path
            or self.parent.resolve(strict=True)!=self.parent
            or self.identity(self.path)!={key:self.expected[key] for key in ('device','inode')}
            or self.identity(self.parent,'parent_')!={
                key:self.expected[key] for key in ('parent_device','parent_inode')}):
            raise ValueError(self.name+' cleanup storage identity changed')
        if self.fd is not None:
            held=os.fstat(self.fd)
            if (held.st_dev,held.st_ino)!=(self.expected['device'],self.expected['inode']):
                raise ValueError(self.name+' held cleanup storage identity changed')

    def __enter__(self):
        self.check()
        try:
            if os.name=='nt':
                import ctypes
                from ctypes import wintypes
                kernel=ctypes.WinDLL('kernel32',use_last_error=True)
                create=kernel.CreateFileW
                create.argtypes=(wintypes.LPCWSTR,wintypes.DWORD,wintypes.DWORD,wintypes.LPVOID,
                                 wintypes.DWORD,wintypes.DWORD,wintypes.HANDLE)
                create.restype=wintypes.HANDLE
                close=kernel.CloseHandle
                close.argtypes=(wintypes.HANDLE,)
                close.restype=wintypes.BOOL
                self.close_handle=close
                # Omitting FILE_SHARE_DELETE freezes both path bindings until CloseHandle.
                for path in (self.parent,self.path):
                    handle=create(str(path),0x80,0x1|0x2,None,3,0x02000000|0x00200000,None)
                    if handle==wintypes.HANDLE(-1).value:
                        raise OSError(ctypes.get_last_error(),'cannot retain cleanup directory')
                    self.handles.append(handle)
            else:
                flags=os.O_RDONLY|getattr(os,'O_DIRECTORY',0)|getattr(os,'O_NOFOLLOW',0)|getattr(os,'O_CLOEXEC',0)
                self.handles.append(os.open(self.parent,flags))
                self.fd=os.open(self.path,flags)
            self.check()
        except BaseException as failure:
            try:self.release()
            except BaseException as close_failure:
                raise RuntimeError('storage admission failed and retained handle release failed: '
                                   +type(close_failure).__name__+': '+str(close_failure)) from failure
            raise
        return self

    def release(self):
        handles=self.handles;descriptor=self.fd;close=self.close_handle
        self.handles=[];self.fd=None;self.close_handle=None
        errors=[]
        if os.name=='nt':
            import ctypes
            if close is None and handles:
                errors.append('missing configured CloseHandle')
            else:
                for handle in reversed(handles):
                    if not close(handle):
                        errors.append('CloseHandle failed '+str(getattr(ctypes,'get_last_error',lambda:0)()))
        else:
            for handle in ([descriptor] if descriptor is not None else [])+list(reversed(handles)):
                try:os.close(handle)
                except OSError as exc:errors.append(type(exc).__name__+': '+str(exc))
        if errors:raise OSError('; '.join(errors))

    def __exit__(self,kind,error,_):
        try:self.release()
        except BaseException as close_failure:
            if error is not None:
                raise RuntimeError('cleanup failed and retained handle release failed: '
                                   +type(close_failure).__name__+': '+str(close_failure)) from error
            raise

    def remove(self,path,directory=False):
        path=Path(path)
        self.check()
        relative=path.relative_to(self.path)
        if relative==Path('.') or '..' in relative.parts:
            raise ValueError('cleanup target escaped retained storage handle')
        if self.fd is not None:
            (os.rmdir if directory else os.unlink)(relative,dir_fd=self.fd)
        else:
            (path.rmdir if directory else path.unlink)()
        self.check()

    def exclusive(self,path,value):
        path=Path(path)
        relative=path.relative_to(self.path)
        if relative==Path('.') or '..' in relative.parts:
            raise ValueError('cleanup receipt escaped retained storage handle')
        data=(json.dumps(value,indent=2,allow_nan=False)+'\n').encode()
        if self.fd is None:
            with path.open('xb') as stream:
                stream.write(data);stream.flush();os.fsync(stream.fileno())
        else:
            descriptor=os.open(relative,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600,dir_fd=self.fd)
            with os.fdopen(descriptor,'wb') as stream:
                stream.write(data);stream.flush();os.fsync(stream.fileno())
        return hashlib.sha256(data).hexdigest()


def cleanup_export(root,service_campaign_root,record):
    root=Path(root)
    service_campaign_root=Path(service_campaign_root)
    output=owned(root,record['probe_output'])
    case_id=record['id']
    if output!=root/(case_id+'-output'):raise ValueError('cleanup case namespace differs')
    request=read_json(output/'request.json')
    service=owned(service_campaign_root,request['service_root'])
    if service!=service_campaign_root/(case_id+'-service'):
        raise ValueError('cleanup service namespace differs')
    if request['phase']!='export' or (request['warmups'],request['repetitions'])!=(2,20):
        raise ValueError('cleanup is restricted to complete22-export timing fixtures')
    verification=owned(root,record['verification_path'])
    verified=read_json(verification,16*1024*1024)
    proof=verified.get('result',{})
    if verified.get('complete') is not True or proof.get('verified') is not True or 'export_artifacts' not in proof.get('coverage',[]):
        raise ValueError('all output oracles must succeed before cleanup')
    for key,name,limit in (('request_sha256','request.json',256*1024),('receipt_sha256','receipt.json',256*1024),('samples_sha256','samples.jsonl',16*1024*1024)):
        if digest(output/name,'sha256',limit)!=proof[key]:raise ValueError('verified ledger changed')
    probe_supervisor=owned(root,record['probe_supervisor_path'])
    verify_supervisor=owned(root,record['verify_supervisor_path'])
    probe_storage=retired_storage(probe_supervisor)
    verify_storage=retired_storage(verify_supervisor)
    if probe_storage!=verify_storage:
        raise ValueError('probe and verifier storage admissions differ')
    storage=probe_storage
    if storage['artifact']['root']!=str(root) or storage['service']['root']!=str(service_campaign_root):
        raise ValueError('cleanup roots differ from supervisor storage admission')
    attempts,values=observations(output);sample_coverage(request,attempts,values)
    encodings={item['path']:item for item in proof['encoded']}
    if len(encodings)!=22 or len(proof['encoded'])!=22:raise ValueError('all22 independent encoded readbacks required')
    roots=[]
    destinations=[];retained=None
    for value in values:
        path=owned(output,value['path'])
        if path.parent!=output:raise ValueError('export destination is not direct owned fixture child')
        independent=encodings.get(str(path))
        if independent is None or digest(path,'sha256',request['encoded_extent'])!=independent['sha256']:
            raise ValueError('verified export changed before cleanup')
        if digest(path,'blake3',request['encoded_extent'])!=value['blake3']:
            raise ValueError('published export changed before cleanup')
        if value['job']['state']!='complete' or value['job']['completed']!=1 or len(value['items'])!=1:
            raise ValueError('export is not durably complete')
        item=value['items'][0];receipt=item['receipt']
        if (item['state']!='published' or receipt['state']!='Published'
            or native_path(receipt['destination'])!=path or receipt['captured_original'] is not None):
            raise ValueError('cleanup requires new-destination publication, no captured original')
        recovery=owned(output,native_path(receipt['recovery_directory']))
        seal=read_json(recovery/'photo-seal.json')
        if (recovery.parent!=output or recovery.name!='.photocatalog-photo-export-'+seal['snapshot']['operation']
            or native_path(seal['snapshot']['destination'])!=path
            or seal['authority_digest']!=item['authority']
            or seal['payload']['digest']!=value['blake3']):
            raise ValueError('recovery directory lacks exact published authority')
        roots.append(recovery)
        if value['iteration']==request['warmups']:
            retained=dict(path=str(path),sha256=independent['sha256'],blake3=value['blake3'])
        else:destinations.append(path)
    if retained is None or len(set(roots))!=22 or len(set(destinations))!=21:
        raise ValueError('cleanup root/retained coverage differs')
    with StorageGuard('artifact',storage['artifact']) as artifact_guard, \
         StorageGuard('service',storage['service']) as service_guard:
        def checked():
            artifact_guard.check();service_guard.check()
        checked()
        delete_files,delete_directories=tree(output,[*roots,*destinations],request['encoded_extent'],
                                           46*request['encoded_extent']+128*1024*1024)
        checked()
        service_files,service_directories=tree(service,[owned(service,service/'catalog')],
                                               request['encoded_extent'],128*1024*1024)
        checked()
        delete_files.extend(service_files);delete_directories.extend(service_directories)
        if len(delete_files)+len(delete_directories)>MAX_ENTRIES:
            raise ValueError('combined cleanup entry bound')
        def guard(path):
            path=Path(path)
            return service_guard if path==service_campaign_root or service_campaign_root in path.parents else artifact_guard
        before=dict(version=3,case_id=case_id,retained=retained,storage=storage,
            verifier_receipt_sha256=digest(verification,'sha256',16*1024*1024),
            probe_supervisor_sha256=digest(probe_supervisor,'sha256',1024*1024),
            verify_supervisor_sha256=digest(verify_supervisor,'sha256',1024*1024),
            probe_supervisor_start_sha256=digest(probe_supervisor.parent/'start.json','sha256',1024*1024),
            verify_supervisor_start_sha256=digest(verify_supervisor.parent/'start.json','sha256',1024*1024),
            delete_files=delete_files,delete_directories=delete_directories,
            catalog_tree_sha256=hashlib.sha256(json.dumps(delete_files,sort_keys=True,separators=(',',':')).encode()).hexdigest())
        start=root/(case_id+'-cleanup-start.json')
        start_sha256=artifact_guard.exclusive(start,before)
        deleted=[];error=None
        try:
            for item in delete_files:
                checked()
                target_guard=guard(item['path'])
                path=owned(target_guard.path,item['path'])
                current=stamp(path)
                if any(current[key]!=item[key] for key in ('bytes','device','inode','mtime_ns')):
                    raise ValueError('cleanup target identity changed after admission')
                # Removing another owned hard link changes ctime. Recheck content
                # instead of mistaking our own earlier unlink for a foreign rewrite.
                if digest(path,'sha256',request['encoded_extent'])!=item['sha256']:
                    raise ValueError('cleanup target bytes changed after admission')
                target_guard.remove(path);deleted.append(str(path))
            for name in delete_directories:
                checked()
                target_guard=guard(name)
                path=owned(target_guard.path,name)
                target_guard.remove(path,directory=True);deleted.append(str(path))
            if digest(retained['path'],'sha256',request['encoded_extent'])!=retained['sha256']:
                raise ValueError('retained first measured export changed')
            if digest(retained['path'],'blake3',request['encoded_extent'])!=retained['blake3']:
                raise ValueError('retained first measured BLAKE3 changed')
        except Exception as exc:error=f'{type(exc).__name__}: {exc}'
        result=dict(version=3,case_id=case_id,complete=error is None,error=error,
            start_sha256=start_sha256,deleted_paths=deleted,retained=retained)
        artifact_guard.exclusive(record['cleanup_path'],result)
        if error:raise RuntimeError('partial cleanup retained; no retry: '+error)
        return result
