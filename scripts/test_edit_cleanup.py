import os
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import edit_cleanup as cleanup
import edit_aggregate


def write_json(path,value):
    Path(path).write_text(json.dumps(value)+'\n')


def descriptor(path):
    path=Path(path)
    value=path.stat();parent=path.parent.stat()
    return dict(root=str(path),device=value.st_dev,inode=value.st_ino,parent=str(path.parent),
                parent_device=parent.st_dev,parent_inode=parent.st_ino,reserve_bytes=0)


class DisposableCleanupAdmission(unittest.TestCase):
    def export_fixture(self,base):
        from blake3 import blake3
        base=Path(base).resolve();root=base/'artifacts';service_root=base/'services'
        root.mkdir();service_root.mkdir()
        case='export-case';output=root/(case+'-output');output.mkdir()
        service=service_root/(case+'-service');(service/'catalog').mkdir(parents=True)
        (service/'catalog/catalog.db').write_bytes(b'disposable catalog')
        request=dict(phase='export',warmups=2,repetitions=20,recipes=[{'exposure':0}],outputs=[{'format':'jpeg'}],
                     encoded_extent=4096,output=str(output),service_root=str(service))
        write_json(output/'request.json',request);write_json(output/'receipt.json',{'probe_complete':True})
        values=[];encoded=[];frames=[]
        for iteration in range(22):
            data=('encoded-'+str(iteration)).encode();destination=output/f'export-{iteration}.jpg'
            destination.write_bytes(data)
            operation='operation-'+str(iteration)
            recovery=output/('.photocatalog-photo-export-'+operation);recovery.mkdir()
            authority=hashlib.sha256(('authority-'+str(iteration)).encode()).hexdigest()
            value=dict(kind='observation',recipe_index=0,iteration=iteration,path=str(destination),
                       blake3=blake3(data).hexdigest(),job=dict(state='complete',completed=1),
                       items=[dict(state='published',authority=authority,receipt=dict(
                           state='Published',destination=str(destination),captured_original=None,
                           recovery_directory=str(recovery)))])
            write_json(recovery/'photo-seal.json',dict(snapshot=dict(operation=operation,destination=str(destination)),
                       authority_digest=authority,payload=dict(digest=value['blake3'])))
            values.append(value)
            encoded.append(dict(path=str(destination),sha256=hashlib.sha256(data).hexdigest()))
            frames.extend((dict(kind='attempt',recipe_index=0,iteration=iteration),value))
        with (output/'samples.jsonl').open('w') as stream:
            for frame in frames:stream.write(json.dumps(frame)+'\n')
        proof=dict(verified=True,coverage=['export_artifacts'],encoded=encoded)
        for key,name in (('request_sha256','request.json'),('receipt_sha256','receipt.json'),
                         ('samples_sha256','samples.jsonl')):
            proof[key]=hashlib.sha256((output/name).read_bytes()).hexdigest()
        verification=root/('verify-'+case+'-verification.json')
        write_json(verification,dict(complete=True,result=proof))
        storage=dict(artifact=descriptor(root),service=descriptor(service_root))
        ownership=dict(known_absent=True,root_reaped=True,root_returncode=0,remaining=[],errors=[])
        supervisors=[]
        for name in (case,'verify-'+case):
            folder=root/name;folder.mkdir()
            write_json(folder/'start.json',dict(limits=dict(storage=storage)))
            write_json(folder/'result.json',dict(complete=True,error=None,ownership=ownership))
            supervisors.append(folder/'result.json')
        record=dict(id=case,probe_output=str(output),verification_path=str(verification),
                    probe_supervisor_path=str(supervisors[0]),verify_supervisor_path=str(supervisors[1]),
                    cleanup_path=str(root/(case+'-cleanup.json')))
        return root,service_root,record,request,dict(complete=True,result=proof),values

    def test_complete_22_output_cleanup_and_aggregate_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root,service,record,request,verification,values=self.export_fixture(temporary)
            result=cleanup.cleanup_export(root,service,record)
            self.assertTrue(result['complete'])
            evidence=edit_aggregate.cleanup_evidence(root,service,record,request,verification,values)
            self.assertEqual(evidence['deleted_files'],44) # 21 outputs, catalog, 22 seal files.
            self.assertEqual(evidence['deleted_directories'],23)
            self.assertTrue(Path(values[2]['path']).is_file())

    def test_service_root_replacement_after_verification_is_retained(self):
        with tempfile.TemporaryDirectory() as temporary:
            root,service,record,_,_,_=self.export_fixture(temporary)
            moved=Path(temporary)/'original-services';service.rename(moved)
            replacement=service;replacement.mkdir()
            (replacement/(record['id']+'-service')/'catalog').mkdir(parents=True)
            marker=replacement/(record['id']+'-service')/'catalog/replacement';marker.write_bytes(b'foreign')
            with self.assertRaisesRegex(ValueError,'storage identity changed'):
                cleanup.cleanup_export(root,service,record)
            self.assertEqual(marker.read_bytes(),b'foreign')

    @unittest.skipIf(os.name=='nt','Windows retained handles prevent the injected rename itself')
    def test_replacement_after_inventory_is_retained_and_cleanup_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root,service,record,_,_,_=self.export_fixture(temporary)
            original_tree=cleanup.tree;calls=0;marker=service/'replacement'
            def replacing(*args,**kwargs):
                nonlocal calls,marker
                result=original_tree(*args,**kwargs);calls+=1
                if calls==2:
                    moved=Path(temporary)/'original-services';service.rename(moved)
                    service.mkdir();marker=service/'replacement';marker.write_bytes(b'foreign')
                return result
            with patch.object(cleanup,'tree',side_effect=replacing), \
                 self.assertRaisesRegex(ValueError,'storage identity changed'):
                cleanup.cleanup_export(root,service,record)
            self.assertEqual(marker.read_bytes(),b'foreign')

    def test_tree_refuses_symlink_and_preserves_outside_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();catalog=root/'catalog';catalog.mkdir()
            source=root/'source';source.write_bytes(b'original')
            try:(catalog/'foreign').symlink_to(source)
            except OSError as exc:self.skipTest(str(exc))
            with self.assertRaises(ValueError):cleanup.tree(root,[catalog],1024,4096)
            self.assertEqual(source.read_bytes(),b'original')

    def test_tree_records_hard_links_as_distinct_paths_without_copying_bytes(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();catalog=root/'catalog';catalog.mkdir()
            source=catalog/'payload';source.write_bytes(b'encoded')
            os.link(source,catalog/'second')
            files,directories=cleanup.tree(root,[catalog],1024,4096)
            self.assertEqual(len(files),2)
            self.assertEqual(files[0]['sha256'],files[1]['sha256'])
            self.assertEqual(files[0]['inode'],files[1]['inode'])
            self.assertEqual(directories,[str(catalog)])

    def test_tree_enforces_declared_logical_bytes_before_deletion(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();catalog=root/'catalog';catalog.mkdir()
            path=catalog/'payload';path.write_bytes(b'12345678')
            with self.assertRaises(ValueError):cleanup.tree(root,[catalog],1024,7)
            self.assertTrue(path.exists())

if __name__=='__main__':unittest.main()
