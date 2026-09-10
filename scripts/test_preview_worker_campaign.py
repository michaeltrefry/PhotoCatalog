"""Source-ready gate tests; no images, child workloads or telemetry sampling."""
import copy
import unittest
from preview_worker_campaign import validate_result, worker_reservation


class WorkerGateTests(unittest.TestCase):
    def result(self):
        return {"complete": True, "fixture_id": "one", "source_blake3": "a" * 64,
                "source_after_blake3": "a" * 64, "renderer_identity": "renderer",
                "metadata": {"width": 12, "height": 18, "orientation": 6},
                "worker_peak_rss_bytes": 128 * 1024 * 1024, "worker_peak_method": "getrusage",
                "artifacts": [{"key": {"edge": edge, "renderer_version": "renderer",
                                        "encoding": {"codec": "jpeg", "quality": 80}},
                               "bytes": 100, "width": 18, "height": 12,
                               "encoded_blake3": "b" * 64, "decoded_blake3": "c" * 64}
                              for edge in (512, 1600)]}

    def test_missing_or_zero_highwater_never_passes(self):
        for missing in (None, 0, True, -1):
            value = self.result()
            value["worker_peak_rss_bytes"] = missing
            with self.assertRaises(ValueError):
                validate_result(value, {"id": "one", "width": 18, "height": 12})

    def test_selected_pair_source_dimensions_and_hashes_are_required(self):
        item = {"id": "one", "width": 18, "height": 12}
        self.assertEqual(validate_result(self.result(), item), 128 * 1024 * 1024)
        for field in ("source_after_blake3", "fixture_id", "complete", "orientation", "quality", "tiers"):
            value = copy.deepcopy(self.result())
            if field == "orientation":
                value["metadata"]["orientation"] = 1
            elif field == "quality":
                value["artifacts"][0]["key"]["encoding"]["quality"] = 65
            elif field == "tiers":
                value["artifacts"].pop()
            else:
                value[field] = None
            with self.assertRaises(ValueError):
                validate_result(value, item)

    def test_reservation_requires_complete_cohort_and_fixed_margin(self):
        mib = 1024 * 1024
        peaks = [128 * mib] * 30
        self.assertEqual(worker_reservation(peaks), 224 * mib)
        for bad in (peaks[:-1], peaks + [1], [None] + peaks[1:], [0] + peaks[1:]):
            with self.assertRaises(ValueError):
                worker_reservation(bad)


if __name__ == "__main__":
    unittest.main()
