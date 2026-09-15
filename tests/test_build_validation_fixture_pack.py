import importlib.util
import json
from pathlib import Path
import sqlite3
import struct
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "scripts/build_validation_fixture_pack.py"
SPEC = importlib.util.spec_from_file_location("build_validation_fixture_pack", SOURCE)
builder = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(builder)


class ValidationFixtureBuilderTests(unittest.TestCase):
    def test_generated_png_is_rgba_and_asymmetric(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "reference.png"
            builder.write_reference_png(path)
            data = path.read_bytes()
            self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
            width, height, depth, color = struct.unpack(">IIBB", data[16:26])
            self.assertEqual((width, height, depth, color), (64, 48, 8, 6))

    def test_synthetic_catalog_uses_migration_schema_and_exact_counts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            originals = root / "originals"
            originals.mkdir()
            path = root / "2014-v13.lrcat"
            counts = builder.create_lightroom_catalog(
                path,
                originals,
                "unit",
                [("2014/January/2014-01-02/a.CR2", "a"), ("2014/January/2014-01-02/b.DNG", "b")],
                1,
                2,
                2,
                [1, 2],
                10.0,
            )
            self.assertEqual(counts["files"], 2)
            self.assertEqual(counts["images"], 3)
            self.assertEqual(counts["virtual_copies"], 1)
            self.assertEqual(counts["catalog_xmp_packets"], 2)
            with sqlite3.connect(str(path)) as db:
                observed = {
                    "files": db.execute("SELECT count(*) FROM AgLibraryFile").fetchone()[0],
                    "images": db.execute("SELECT count(*) FROM Adobe_images").fetchone()[0],
                    "collections": db.execute("SELECT count(*) FROM AgLibraryCollection").fetchone()[0],
                    "xmp": db.execute("SELECT count(*) FROM Adobe_AdditionalMetadata").fetchone()[0],
                }
                packet = db.execute("SELECT xmp FROM Adobe_AdditionalMetadata ORDER BY id_local LIMIT 1").fetchone()[0]
            self.assertEqual(observed, {"files": 2, "images": 3, "collections": 2, "xmp": 2})
            length = int.from_bytes(packet[:4], "big")
            self.assertEqual(len(__import__("zlib").decompress(packet[4:])), length)

    def test_manifest_template_keeps_release_readiness_false(self):
        template = json.loads((ROOT / "docs/VALIDATION_FIXTURE_MANIFEST.template.json").read_text())
        readiness = template["readiness"]
        self.assertTrue(readiness["fixture_pack_built"])
        self.assertFalse(readiness["final_installer_supplied"])
        self.assertFalse(readiness["ready_for_release_handoff"])


if __name__ == "__main__":
    unittest.main()
