"""Adversarial checks for evidence that must not certify the wrong source."""
import copy
import unittest

from validate_image_corpus import validate_reference, validate_render


class CorpusEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.row = {
            "reference_source": "independent TIFF header inspection",
            "expected_format": "DNG", "expected_source_bits": 16,
            "expected_camera_make": "Canon", "expected_camera_model": "EOS",
            "reference": {"width": 30, "height": 20},
        }
        self.value = {
            "status": "decoded", "width": 30, "height": 20,
            "metadata": {"format": "DNG", "camera_make": "Canon",
                         "camera_model": "EOS",
                         "preview_source": "full-quality original rendering"},
            "provenance": {"source_bits_per_channel": 16},
            "nonfinite_components": 0,
            "alpha_zero": 0, "alpha_partial": 0, "alpha_opaque": 600,
        }

    def test_matching_reference(self):
        validate_reference(self.row)
        validate_render(self.row, self.value)

    def test_no_optional_reference_escape(self):
        for key in self.row:
            with self.subTest(key=key), self.assertRaises(ValueError):
                row = copy.deepcopy(self.row)
                del row[key]
                validate_reference(row)

    def test_wrong_format_precision_camera_preview_rejected(self):
        cases = [("metadata", "format", "PNG"),
                 ("metadata", "camera_make", "Other"),
                 ("metadata", "camera_model", None),
                 ("metadata", "preview_source", "embedded JPEG"),
                 ("provenance", "source_bits_per_channel", 8)]
        for section, key, replacement in cases:
            with self.subTest(key=key), self.assertRaises(ValueError):
                value = copy.deepcopy(self.value)
                value[section][key] = replacement
                validate_render(self.row, value)

    def test_thumbnail_dimensions_rejected_even_with_valid_alpha(self):
        self.value.update(width=3, height=2, alpha_opaque=6)
        with self.assertRaises(ValueError):
            validate_render(self.row, self.value)

    def test_absent_camera_requires_explicit_evidence(self):
        self.row["expected_camera_make"] = None
        with self.assertRaises(ValueError):
            validate_reference(self.row)
        self.row["camera_reference_note"] = "Independent header has no Make tag"
        validate_reference(self.row)


if __name__ == "__main__":
    unittest.main()
