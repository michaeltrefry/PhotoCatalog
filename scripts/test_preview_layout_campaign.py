"""Small layout receipt contracts; no filesystem corpus or timing execution."""
import copy
import unittest
from preview_layout_campaign import compare, plan, validate_lookup


class LayoutCampaignTests(unittest.TestCase):
    def receipt(self):
        return {"complete":True,"dataset_blake3":"a"*64,
                "footprint_before":{"thumbnail":{"object_files":10000}},
                "footprint_after":{"thumbnail":{"object_files":10000}},
                "passes":[{"pass":i,"complete":True,"lookup_count":10000,
                           "distinct_actual_read_content_hashes":10000,"bytes_read_verified":1000000,
                           "seed":None if i<3 else 22841+i-3,
                           "order":"sequential" if i<3 else "seeded_random",
                           "samples_file":f"pass-{i}-samples.json"} for i in range(6)]}

    def test_fixed_two_counts_two_independent_layouts(self):
        self.assertEqual(plan(),[(10000,"flat"),(10000,"hash-prefix"),(100000,"flat"),(100000,"hash-prefix")])

    def test_missing_actual_files_dedup_and_wrong_trace_are_rejected(self):
        validate_lookup(self.receipt(),10000)
        for field in ("files","unique","count","seed","path","incomplete","unbound"):
            value=copy.deepcopy(self.receipt())
            if field=="files": value["footprint_after"]["thumbnail"]["object_files"]=30
            elif field=="unique": value["passes"][0]["distinct_actual_read_content_hashes"]=30
            elif field=="count": value["passes"][0]["lookup_count"]=9999
            elif field=="seed": value["passes"][3]["seed"]=1
            elif field=="path": value["passes"][0]["samples_file"]="../wrong"
            elif field=="incomplete": value["passes"][0]["complete"]=False
            else: value["dataset_blake3"]=None
            with self.assertRaises(ValueError): validate_lookup(value,10000)

    def test_aggregate_rejects_missing_groups_and_different_payload_totals(self):
        preparations, lookups = [], []
        for count, layout in plan():
            result = self.receipt()
            for phase in ("footprint_before", "footprint_after"):
                result[phase]["thumbnail"]["object_files"] = count
            for row in result["passes"]:
                row.update(lookup_count=count, distinct_actual_read_content_hashes=count,
                           bytes_read_verified=count*100, wall_ms=1., independent_verification_ms=.2)
            preparations.append({"count":count,"layout":layout,"complete":True,
                "result":{"encoded_object_bytes":count*100,"footprint":result["footprint_before"]}})
            lookups.append({"count":count,"layout":layout,"complete":True,"result":result,"distributions":[]})
        self.assertEqual(set(compare(preparations,lookups)), {"10000","100000"})
        with self.assertRaises(ValueError): compare(preparations[:-1],lookups)
        wrong = copy.deepcopy(preparations)
        wrong[0]["result"]["encoded_object_bytes"] += 1
        with self.assertRaises(ValueError): compare(wrong,lookups)
        duplicate = copy.deepcopy(lookups)
        duplicate[-1] = duplicate[0]
        with self.assertRaises(ValueError): compare(preparations,duplicate)


if __name__=="__main__": unittest.main()
