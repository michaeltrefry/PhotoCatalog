import unittest
import edit_request as expansion
import edit_correctness_matrix as matrix
import edit_qualification as qualification


class RequestExpansion(unittest.TestCase):
    def test_metadata_and_case_allocation_overrides_are_preserved(self):
        case=matrix.output_matrix()[0]
        source=dict(id=case['fixture_id'],path='/private/source',sha256='a'*64,blake3='b'*64,width=48,height=32)
        limits=dict(decode={'max_encoded_bytes':1},render={'max_pixels':2},encoded_extent=512*qualification.MIB)
        request=expansion.expand_request(case,source,limits,'/private/artifacts','/private/services','/private/worker')
        self.assertEqual(request['version'],2)
        self.assertEqual(request['output'],'/private/artifacts/'+case['id']+'-output')
        self.assertEqual(request['service_root'],'/private/services/'+case['id']+'-service')
        self.assertEqual(request['encoded_extent'],qualification.MIB)
        self.assertEqual(request['metadata']['xmp'],case['metadata']['xmp'])
        self.assertEqual(set(request['metadata']['exif']),set(expansion.EXIF_FIELDS))
        self.assertFalse(request['resolve_embedded'])
        self.assertIsNone(request['background_source'])

    def test_generated_large_override_and_selected_metadata_are_explicit(self):
        case=matrix.support_matrix()[0]
        source=dict(id=case['fixture_id'],path='/private/source',sha256='a'*64,blake3='b'*64,width=10000,height=10000)
        limits=dict(decode={},render={},encoded_extent=1)
        request=expansion.expand_request(case,source,limits,'/private/artifacts','/private/services','/private/worker')
        self.assertEqual(request['decode'],case['limits']['decode'])
        self.assertEqual(request['render'],case['limits']['render'])
        self.assertEqual(request['metadata'],expansion.normalized_metadata({}))

    def test_aliasing_service_and_artifact_roots_are_rejected(self):
        case=matrix.output_matrix()[0]
        source=dict(id=case['fixture_id'],path='/private/source',sha256='a'*64,blake3='b'*64,width=48,height=32)
        limits=dict(decode={},render={},encoded_extent=1)
        for service in ('/private/artifacts','/private/artifacts/nested','/private'):
            with self.subTest(service=service),self.assertRaisesRegex(ValueError,'disjoint'):
                expansion.expand_request(case,source,limits,'/private/artifacts',service,'/private/worker')

if __name__=='__main__':unittest.main()
