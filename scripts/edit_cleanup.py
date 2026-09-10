"""Delete only verified disposable export fixtures, preserving sealed evidence.

This is not a product garbage collector. Exact published receipts authorize each
recovery directory; unknown prefix matches, symlinks and failed jobs are retained.
"""
import hashlib
import json
import os
from pathlib import Path
import stat
from edit_verify import digest,read_json,observations,sample_coverage,owned

MAX_ENTRIES=4096


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


def cleanup_export(root,record):
    root=Path(root).resolve(strict=True)
    output=owned(root,record['probe_output'])
    case_id=record['id']
    if output!=root/(case_id+'-output'):raise ValueError('cleanup case namespace differs')
    request=read_json(output/'request.json')
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
    retired(probe_supervisor);retired(verify_supervisor)
    attempts,values=observations(output);sample_coverage(request,attempts,values)
    encodings={item['path']:item for item in proof['encoded']}
    if len(encodings)!=22 or len(proof['encoded'])!=22:raise ValueError('all22 independent encoded readbacks required')
    roots=[owned(output,output/'catalog')]
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
        if item['state']!='published' or receipt['state']!='Published' or receipt['destination']!=str(path) or receipt['captured_original'] is not None:
            raise ValueError('cleanup requires new-destination publication, no captured original')
        recovery=owned(output,receipt['recovery_directory'])
        seal=read_json(recovery/'photo-seal.json')
        if (recovery.parent!=output or recovery.name!='.photocatalog-photo-export-'+seal['snapshot']['operation']
            or seal['snapshot']['destination']!=str(path) or seal['authority_digest']!=item['authority']
            or seal['payload']['digest']!=value['blake3']):
            raise ValueError('recovery directory lacks exact published authority')
        roots.append(recovery)
        if value['iteration']==request['warmups']:
            retained=dict(path=str(path),sha256=independent['sha256'],blake3=value['blake3'])
        else:destinations.append(path)
    if retained is None or len(set(roots))!=23 or len(set(destinations))!=21:
        raise ValueError('cleanup root/retained coverage differs')
    delete_files,delete_directories=tree(output,[*roots,*destinations],request['encoded_extent'],
                                       46*request['encoded_extent']+128*1024*1024)
    before=dict(version=1,case_id=case_id,retained=retained,
        verifier_receipt_sha256=digest(verification,'sha256',16*1024*1024),
        probe_supervisor_sha256=digest(probe_supervisor,'sha256',1024*1024),
        verify_supervisor_sha256=digest(verify_supervisor,'sha256',1024*1024),
        delete_files=delete_files,delete_directories=delete_directories,
        catalog_tree_sha256=hashlib.sha256(json.dumps(delete_files,sort_keys=True,separators=(',',':')).encode()).hexdigest())
    start=root/(case_id+'-cleanup-start.json');exclusive(start,before)
    deleted=[];error=None
    try:
        for item in delete_files:
            path=owned(output,item['path'])
            current=stamp(path)
            if any(current[key]!=item[key] for key in ('bytes','device','inode','mtime_ns')):
                raise ValueError('cleanup target identity changed after admission')
            # Removing another owned hard link changes ctime. Recheck content
            # instead of mistaking our own earlier unlink for a foreign rewrite.
            if digest(path,'sha256',request['encoded_extent'])!=item['sha256']:
                raise ValueError('cleanup target bytes changed after admission')
            path.unlink();deleted.append(str(path))
        for name in delete_directories:
            path=owned(output,name)
            path.rmdir();deleted.append(str(path))
        if digest(retained['path'],'sha256',request['encoded_extent'])!=retained['sha256']:
            raise ValueError('retained first measured export changed')
        if digest(retained['path'],'blake3',request['encoded_extent'])!=retained['blake3']:
            raise ValueError('retained first measured BLAKE3 changed')
    except Exception as exc:error=f'{type(exc).__name__}: {exc}'
    result=dict(version=1,case_id=case_id,complete=error is None,error=error,
        start_sha256=digest(start,'sha256',16*1024*1024),deleted_paths=deleted,retained=retained)
    exclusive(record['cleanup_path'],result)
    if error:raise RuntimeError('partial cleanup retained; no retry: '+error)
    return result
