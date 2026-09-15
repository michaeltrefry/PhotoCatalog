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
                builder.PORTABLE_ROOT_BINDING,
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
                root_binding = db.execute("SELECT absolutePath FROM AgLibraryRootFolder").fetchone()[0]
            self.assertEqual(observed, {"files": 2, "images": 3, "collections": 2, "xmp": 2})
            self.assertEqual(root_binding, builder.PORTABLE_ROOT_BINDING)
            length = int.from_bytes(packet[:4], "big")
            self.assertEqual(len(__import__("zlib").decompress(packet[4:])), length)

    def test_manifest_template_keeps_release_readiness_false(self):
        template = json.loads((ROOT / "docs/VALIDATION_FIXTURE_MANIFEST.template.json").read_text())
        readiness = template["readiness"]
        self.assertFalse(template["generation"]["absolute_paths_are_generated_for_this_pack"])
        self.assertTrue(readiness["fixture_pack_built"])
        self.assertTrue(readiness["portable_catalog_roots_unbound"])
        self.assertFalse(readiness["target_destination_counts_qualified"])
        self.assertFalse(readiness["final_installer_supplied"])
        self.assertFalse(readiness["ready_for_release_handoff"])
        qualification = json.loads(
            (ROOT / "docs/VALIDATION_TARGET_QUALIFICATION.template.json").read_text()
        )
        self.assertEqual(
            qualification["status"],
            "pending_root_native_import_and_migration_qualification",
        )
        self.assertFalse(qualification["approved_for_platform_handoff"])

    def test_working_freeze_rebinds_catalog_and_only_authorizes_declared_sidecars(self):
        with tempfile.TemporaryDirectory() as temporary:
            baseline = Path(temporary) / "baseline"
            working = Path(temporary) / "working"
            originals = baseline / "originals"
            catalogs = baseline / "lightroom-catalogs"
            originals.mkdir(parents=True)
            catalogs.mkdir(parents=True)
            photo = originals / "2014/January/2014-01-02/a.CR2"
            photo.parent.mkdir(parents=True)
            photo.write_bytes(b"immutable photo")
            sidecar = photo.with_suffix(".xmp")
            sidecar.write_text("baseline sidecar")
            catalog = catalogs / "2014-v13.lrcat"
            builder.create_lightroom_catalog(
                catalog,
                builder.PORTABLE_ROOT_BINDING,
                "working-unit",
                [("2014/January/2014-01-02/a.CR2", "a")],
                None,
                1,
                1,
                [],
                10.0,
            )
            (baseline / builder.PACK_MARKER).write_text(f"{builder.PACK_VERSION}\n")
            (baseline / "README.md").write_text("portable fixture")
            source_reconciliation = {
                "source_inventory": {"formats": {"CR2": 1}},
                "target_qualification": {
                    "status": "pending_root_native_import_and_migration_qualification"
                },
            }
            (baseline / "expected-reconciliation.json").write_text(
                json.dumps(source_reconciliation, sort_keys=True) + "\n"
            )
            (baseline / "target-qualification.template.json").write_text(
                json.dumps({"approved_for_platform_handoff": False}) + "\n"
            )
            records = builder.file_records(
                baseline,
                {
                    sidecar.relative_to(baseline).as_posix(): {"format": "XMP"},
                    photo.relative_to(baseline).as_posix(): {"format": "CR2"},
                },
            )
            manifest = {
                "format_version": builder.PACK_VERSION,
                "pack_id": "unit-portable-baseline",
                "public_fixture_provenance": {"sha256": builder.sha256(photo)},
                "counts": {"source_files_hashed": len(records)},
                "source_reconciliation": source_reconciliation,
                "files": records,
                "readiness": {
                    "portable_catalog_roots_unbound": True,
                    "target_destination_counts_qualified": False,
                },
            }
            manifest_path = baseline / "fixture-manifest.json"
            manifest_path.write_text(json.dumps(manifest, sort_keys=True) + "\n")
            (baseline / "fixture-manifest.sha256").write_text(
                f"{builder.sha256(manifest_path)}  fixture-manifest.json\n"
            )

            builder.prepare_working_copy(baseline, working, False)
            working_manifest = json.loads((working / "working-manifest.json").read_text())
            binding = str((working / "inputs/originals").resolve()) + __import__("os").sep
            self.assertEqual(working_manifest["bound_originals_root"], binding)
            self.assertEqual(len(working_manifest["authorized_mutations"]), 1)
            self.assertTrue((working / "outputs/export/existing-export.jpg").is_file())
            builder.verify_working_copy(working, None, False)
            originals = working / "inputs/originals"
            photo = originals / "2014/January/2014-01-02/a.CR2"
            sidecar = photo.with_suffix(".xmp")
            relocated = working / "outputs/relink/originals"
            relocated.parent.mkdir(parents=True, exist_ok=True)
            originals.rename(relocated)
            relocated_sidecar = relocated / sidecar.relative_to(originals)
            relocated_sidecar.write_text("approved replacement sidecar")
            with self.assertRaisesRegex(RuntimeError, "checksum/size mismatch"):
                builder.verify_working_copy(working, relocated, False)
            report = builder.verify_working_copy(working, relocated, True)
            self.assertEqual(len(report["authorized_sidecar_changes"]), 1)
            relocated_photo = relocated / photo.relative_to(originals)
            relocated_photo.write_bytes(b"changed photo")
            with self.assertRaisesRegex(RuntimeError, "checksum/size mismatch"):
                builder.verify_working_copy(working, relocated, True)


if __name__ == "__main__":
    unittest.main()
