"""Draft source-contract tests. No image reads or subprocess execution."""
import copy
import unittest
import xml.etree.ElementTree as ET
import edit_qualification as q
import edit_correctness_matrix


def manifest():
    return {"version": 1, "inputs": [
        {"id": key, "path": f"/owned/{key}.image", "sha256": "a" * 64,
         "width": 512, "height": 512} for key in q.IDS]}


class MatrixContract(unittest.TestCase):
    def test_controlled_metadata_retains_nested_qualifier_in_valid_rdf_form(self):
        rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'
        unknown='https://photocatalog.invalid/qualification/1/'
        for extended in (False,True):
            root=ET.fromstring(edit_correctness_matrix.metadata(extended)['xmp'])
            description=root.find('.//{'+rdf+'}Description')
            self.assertEqual(description.get('{'+rdf+'}about'),'urn:photocatalog:qualification:subject')
            child=description.find('{'+unknown+'}structure/{'+unknown+'}child')
            self.assertEqual(child.attrib,{'{'+rdf+'}parseType':'Resource'})
            self.assertFalse((child.text or '').strip())
            self.assertEqual(child.find('{'+rdf+'}value').text,'value')
            self.assertEqual(child.find('{'+unknown+'}flag').text,'yes')

    def test_all_operations_receive_own_fixed_sample_counts(self):
        value = q.plan(manifest())
        self.assertEqual(len(value["cases"]), 337)
        for fixture in q.TIMING:
            cases = [c for c in value["cases"] if c["fixture_id"] == fixture]
            kernel = [c for c in cases if c["phase"] == "kernel"]
            service = [c for c in cases if c["phase"] == "warm_service"]
            self.assertEqual({c["operation"] for c in kernel}, set(q.recipes()))
            self.assertEqual({c["operation"] for c in service}, set(q.recipes()) - {"neutral"})
            self.assertTrue(all((c["warmups"], c["repetitions"]) == (2, 100)
                                for c in kernel + service))
            self.assertTrue(all(c["recipes"][0] != c["recipes"][1] for c in service))

    def test_fixed_export_matrix_and_case_id_uniqueness(self):
        value = q.plan(manifest())
        ids = [c["id"] for c in value["cases"]]
        self.assertEqual(len(ids), len(set(ids)))
        for fixture in q.EXPORT_TIMING:
            cases = [c for c in value["cases"] if c["fixture_id"] == fixture and c["phase"] == "export"]
            self.assertEqual({c["operation"] for c in cases}, set(q.outputs()))
            self.assertTrue(all((c["warmups"], c["repetitions"]) == (2, 20) for c in cases))
        self.assertEqual(q.outputs()["tiff32"]["profile"], {"kind": "linear_srgb"})
        self.assertEqual(q.outputs()["jpeg8"]["alpha"]["mode"], "composite")

    def test_wrong_cohort_duplicate_hash_and_dimensions_rejected(self):
        original = manifest()
        changes = [lambda v: v["inputs"].pop(),
                   lambda v: v["inputs"][0].update(id=v["inputs"][1]["id"]),
                   lambda v: v["inputs"][0].update(sha256="z"*64),
                   lambda v: v["inputs"][0].update(width=True),
                   lambda v: v["inputs"][0].update(width=10000, height=10000)]
        for mutate in changes:
            value = copy.deepcopy(original)
            mutate(value)
            with self.assertRaises(ValueError):
                q.plan(value)

    def test_plans_do_not_claim_execution_or_drop_pending_scope(self):
        value = q.plan(manifest())
        self.assertEqual(value["status"], "prospective_source_checkpoint_not_admitted")
        self.assertEqual(value["admission"]["automatic_retries"], 0)
        self.assertEqual(len(value["pending"]), 6)
        self.assertEqual(q.recipe()["settings"]["white_balance"], {"mode": "as_shot"})
        self.assertEqual(q.recipes()["white_balance"][0]["settings"]["white_balance"]["mode"], "temperature_tint")


if __name__ == "__main__":
    unittest.main()
