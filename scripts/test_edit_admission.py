import unittest
from pathlib import Path
from edit_admission import validate_record_paths


class ReceiptPathAdmission(unittest.TestCase):
    def test_receipts_cannot_escape_or_alias_a_different_case(self):
        root=Path('/private/campaign')
        case={'id':'export-example','phase':'export'}
        record=dict(probe_output=str(root/'export-example-output'),
            request_path=str(root/'export-example-output/request.json'),
            verification_path=str(root/'verify-export-example-verification.json'),
            probe_supervisor_path=str(root/'export-example/result.json'),
            verify_supervisor_path=str(root/'verify-export-example/result.json'),
            cleanup_path=str(root/'export-example-cleanup.json'))
        validate_record_paths(record,root,case)
        for name in record:
            for value in ('/private/unrelated/result.json',str(root/'different-case/result.json'),None):
                with self.subTest(name=name,value=value),self.assertRaises(ValueError):
                    validate_record_paths(dict(record,**{name:value}),root,case)
        case['phase']='correctness'
        with self.assertRaises(ValueError):validate_record_paths(record,root,case)
        validate_record_paths(dict(record,cleanup_path=None),root,case)


if __name__=='__main__':unittest.main()
