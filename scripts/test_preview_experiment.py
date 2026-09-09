import copy
import struct
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch
import generate_preview_fixtures as fixtures
import preview_experiment as campaign

class ProtocolTests(unittest.TestCase):
    def manifest(self):
        result={"version":1,"inputs":[]}
        for kind,count in [("private",22),("public",2),("preparation",6)]:
            for n in range(count):
                item={"id":f"{kind}-{n}","kind":kind,"group":kind,"path":f"/{kind}-{n}.png","sha256":"0"*64,"width":1,"height":1}
                if kind=="preparation":item["oracle"]={str(edge):{} for edge in campaign.EDGES}
                result["inputs"].append(item)
        return result
    def test_manifest_requires_exact_independent_fixture_coverage(self):
        valid=self.manifest();campaign.validate_manifest(valid)
        for change in ('duplicate','count','oracle','path-id','digest','dimensions'):
            value=copy.deepcopy(valid)
            if change=='duplicate':value['inputs'][1]['id']=value['inputs'][0]['id']
            if change=='count':value['inputs'].pop()
            if change=='oracle':value['inputs'][-1]['oracle'].pop('256')
            if change=='path-id':value['inputs'][0]['id']='../escape'
            if change=='digest':value['inputs'][0]['sha256']='x'*64
            if change=='dimensions':value['inputs'][0]['width']=0
            with self.subTest(change=change),self.assertRaises((ValueError,KeyError)):campaign.validate_manifest(value)
    def test_independent_oracles_cover_alpha_and_integer_average(self):
        self.assertEqual(fixtures.prepared([0,0,0,0],8,4),bytes([255,255,255]))
        self.assertEqual(fixtures.prepared([0,0,0,255],8,4),bytes([0,0,0]))
        self.assertEqual(fixtures.prepared([0,0,0,128],8,4),bytes([187,187,187]))
        w,h,data=fixtures.reduce_integer(bytes([0]*3+[255]*3+[255]*3+[0]*3),2,2,1)
        self.assertEqual((w,h,data),(1,1,bytes([128]*3)))
    def test_orientation_chunk_has_independent_little_endian_short(self):
        data=fixtures.png(2,3,8,3,[0]*18,6)
        start=data.index(b'eXIf')+4
        self.assertEqual(data[start:start+10],b'II\x2a\0\x08\0\0\0\x01\0')
        self.assertEqual(struct.unpack('<HHIHHI',data[start+10:start+26]),(274,3,1,6,0,0))
    def test_lane_is_required_before_any_file_work(self):
        class Args:lane_token='not-authorized'
        with patch.object(campaign.Path,'resolve',side_effect=AssertionError('file access')):
            with self.assertRaises(ValueError):campaign.run(Args())
    def test_receipt_validation_recomputes_samples_and_checks_artifacts(self):
        with tempfile.TemporaryDirectory() as temp:
            folder=Path(temp)
            for n in range(3):(folder/f'encode-{n}.webp').write_bytes(b'fixture')
            encode={**campaign.stats([1.,2.,3.]),'samples_ms':[1.,2.,3.]}
            decode={**campaign.stats([1.]*20),'samples_ms':[1.]*20}
            surface={'width':1,'height':1,'rgb_blake3':'fixture'}
            value={'complete':True,'version':1,'edge':256,**surface,'prepared_blake3':'fixture',
                'settings':{'codec':'webp','quality':65},'encode':encode,'decode':decode,
                'artifacts':[{'bytes':7} for _ in range(3)],'quality':{'mse_rgb8':0.,'block_rgb_ssim':1.}}
            campaign.validate_case(value,256,'webp',65,surface,folder)
            for change in ('samples','summary','settings','bytes','surface'):
                bad=copy.deepcopy(value)
                if change=='samples':bad['decode']['samples_ms'].pop()
                if change=='summary':bad['encode']['p95_ms']=0.
                if change=='settings':bad['settings']['quality']=50
                if change=='bytes':bad['artifacts'][0]['bytes']=10
                if change=='surface':bad['prepared_blake3']='wrong'
                with self.subTest(change=change),self.assertRaises(ValueError):campaign.validate_case(bad,256,'webp',65,surface,folder)

if __name__=='__main__':unittest.main()
