#!/usr/bin/env python3
"""Exercise the real CLI with owned APFS disk images; never detach user storage."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output-parent", required=True, type=Path)
    args = parser.parse_args()
    assert sys.platform == "darwin", "requires macOS"
    binary = args.binary.resolve(strict=True)
    fixture = Path(__file__).resolve().parent.parent / "tests/fixtures/generated-swatches.png"
    args.output_parent.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="sc-22840-mounted-", dir=args.output_parent))
    receipt = {"complete": False, "binary_sha256": sha(binary), "fixture_sha256": sha(fixture), "steps": []}
    mounted = {}

    def run(argv):
        result = subprocess.run([str(x) for x in argv], capture_output=True, check=False)
        if result.returncode:
            raise RuntimeError(f"{argv[0]} failed ({result.returncode}): {result.stderr.decode(errors='replace')}")
        return result.stdout

    def cli(*argv):
        output = run([binary, "--catalog", root / "catalog", *argv])
        return json.loads(output) if output.strip() else None

    def record(name, value):
        receipt["steps"].append({"name": name, "value": value})
        (root / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")

    def attach(image, point):
        point.mkdir(exist_ok=True)
        data = plistlib.loads(run(["hdiutil", "attach", image, "-mountpoint", point, "-nobrowse", "-plist"]))
        entries = [e for e in data["system-entities"] if e.get("mount-point") == str(point)]
        assert len(entries) == 1, "expected one owned mounted volume"
        mounted[str(image)] = (entries[0]["dev-entry"], point)
        record("attach", {"image": str(image), "point": str(point), "device": entries[0]["dev-entry"]})

    def detach(image):
        device, point = mounted[str(image)]
        info = plistlib.loads(run(["hdiutil", "info", "-plist"]))
        owned = [i for i in info["images"] if Path(i["image-path"]).resolve() == image.resolve()]
        assert len(owned) == 1, "image ownership changed; refuse detach"
        assert any(e.get("dev-entry") == device and e.get("mount-point") == str(point)
                   for e in owned[0]["system-entities"]), "device/mount ownership changed; refuse detach"
        run(["hdiutil", "detach", device])
        del mounted[str(image)]
        record("detach", {"image": str(image), "device": device})

    def create_image(name):
        image = root / (name + ".sparseimage")
        run(["hdiutil", "create", "-size", "256m", "-fs", "APFS", "-type", "SPARSE", "-volname", name, image])
        return image

    def preview(asset, name):
        path = root / (name + ".jpg")
        cli("preview", asset, path)
        return sha(path)

    def field_evidence(view):
        fields = json.loads(json.dumps(view["fields"]))
        for field in fields:
            for candidate in field["candidates"]:
                # Current display locators intentionally change during relinking;
                # every source/model/observation/value identity must remain equal.
                del candidate["source_display"]
        return fields

    def map_folder(old, new):
        plan = cli("relink-folder", old, new)
        plan = cli("relink-prepare", plan["id"], "--limit", "1")
        while plan["state"] == "preparing":
            plan = cli("relink-prepare", plan["id"], "--limit", "1")
        assert plan["matched"] == 1 and plan["unresolved"] == plan["unresolved_sources"] == 0, plan
        record("prepared", plan)
        assert cli("relink-apply", plan["id"])["state"] == "applied"
        return plan["id"]

    try:
        image = create_image("PhotoCatalogFixtureA")
        a, b = root / "mount-a", root / "mount-b"
        attach(image, a)
        photos = a / "photos"
        photos.mkdir()
        original = photos / "été.png"
        shutil.copyfile(fixture, original)
        sidecar = original.with_suffix(".xmp")
        sidecar.write_text('<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3"/></rdf:RDF></x:xmpmeta>')
        source_hashes = {p.name: sha(p) for p in (original, sidecar)}
        assert cli("import", photos)["imported"] == 1
        rows = cli("browse")
        assert len(rows) == 1
        asset = rows[0]["id"]
        before = cli("metadata", asset)
        rating = next(f for f in before["fields"] if f["name"] == "rating")
        model = rating["candidates"][0]["model_id"]
        cli("metadata-resolve", asset, "rating", model, "--expected-revision", str(before["revision"]))
        before = cli("metadata", asset)
        preview_hash = preview(asset, "before")
        volume_a = cli("storage-locate", original)
        record("initial", {"asset": asset, "metadata": before, "volume": volume_a, "preview_sha256": preview_hash})
        detach(image)
        offline = cli("storage-status", asset)
        assert offline["state"] == "offline", offline
        assert preview(asset, "offline") == preview_hash
        assert cli("metadata", asset)["fields"] == before["fields"]
        record("offline", offline)
        attach(image, b)
        report = cli("import", b / "photos")
        assert report["unchanged"] == 1 and report["metadata_updated"] == 0, report
        assert [r["id"] for r in cli("browse")] == [asset]
        after = cli("metadata", asset)
        assert field_evidence(after) == field_evidence(before), (before, after)
        assert all(str(b) in s["display"] for s in after["sources"])
        assert preview(asset, "reconnected") == preview_hash
        record("reconnected", {"report": report, "metadata": after, "status": cli("storage-status", asset)})
        reconnect_plan = cli("relink-plans")[0]["id"]
        assert cli("relink-undo", reconnect_plan)["state"] == "undone"
        assert cli("get", asset)["original_path"] == str(original)
        assert preview(asset, "undo-reconnect") == preview_hash
        map_folder(a / "photos", b / "photos")
        (b / "photos").rename(b / "reorganized")
        assert cli("storage-status", asset)["state"] == "missing"
        plan = map_folder(b / "photos", b / "reorganized")
        assert cli("get", asset)["original_path"] == str(b / "reorganized" / original.name)
        assert cli("relink-undo", plan)["state"] == "undone"
        assert cli("get", asset)["original_path"] == str(b / "photos" / original.name)
        assert preview(asset, "undo-folder") == preview_hash
        map_folder(b / "photos", b / "reorganized")
        copies = root / "upgrade-copy"
        shutil.copytree(b / "reorganized", copies)
        for path in copies.iterdir():
            assert sha(path) == source_hashes[path.name]
        old_volume = cli("storage-locate", b / "reorganized" / original.name)["volume"]["persistent_identity"]
        detach(image)
        upgraded = create_image("PhotoCatalogFixtureB")
        attach(upgraded, b)
        shutil.copytree(copies, b / "reorganized")
        for path in (b / "reorganized").iterdir():
            st = path.stat()
            os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns + 60_000_000_000))
        new_volume = cli("storage-locate", b / "reorganized" / original.name)["volume"]["persistent_identity"]
        assert old_volume != new_volume
        report = cli("import", b / "reorganized")
        assert report["unchanged"] == 1 and report["metadata_updated"] == 0, report
        assert [r["id"] for r in cli("browse")] == [asset]
        latest = cli("metadata", asset)
        assert next(f for f in latest["fields"] if f["name"] == "rating")["selected_model"] == model
        instances = cli("metadata-file-instances", asset)
        assert instances, "new file-instance evidence missing"
        assert preview(asset, "upgraded") == preview_hash
        for path in (b / "reorganized").iterdir():
            assert sha(path) == source_hashes[path.name]
        record("upgraded", {"old_volume": old_volume, "new_volume": new_volume, "report": report, "file_instances": instances})
        assert sha(binary) == receipt["binary_sha256"] and sha(fixture) == receipt["fixture_sha256"]
        receipt["complete"] = True
    except BaseException as error:
        receipt["error"] = str(error)
        raise
    finally:
        for image_path in list(mounted):
            try:
                detach(Path(image_path))
            except BaseException as error:
                receipt["complete"] = False
                receipt.setdefault("cleanup_errors", []).append(str(error))
        receipt["remaining_owned_mounts"] = mounted
        (root / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        print(root / "receipt.json")
    assert receipt["complete"], receipt


if __name__ == "__main__":
    main()
