#!/usr/bin/env python3
"""Build a disposable cross-platform LensWorks validation pack.

The output contains only generated/public fixtures and synthetic Lightroom data.
It never reads a user's catalog or photo library. Output defaults beneath .deps,
which is ignored by Git.
"""

from __future__ import annotations

import argparse
import binascii
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sqlite3
import struct
import subprocess
import tempfile
import urllib.request
import zlib


SCRIPT = Path(__file__).resolve()
REPOSITORY = SCRIPT.parents[1]
PACK_VERSION = 1
PACK_MARKER = ".lensworks-validation-fixture-pack"
MAX_RAW_BYTES = 64 * 1024 * 1024
TRACKED_FIXTURES = {
    "generated-linear-mask.dng": (
        REPOSITORY / "tests/fixtures/generated-linear-mask.dng",
        "610106e5d9ef60e0422811f03c895e3c73fa3b2624d3be6bba3d817655263474",
    ),
    "generated-swatches-10bit.avif": (
        REPOSITORY / "tests/fixtures/generated-swatches-10bit.avif",
        "ca500698f13307753234103a85c6804d69dddea9db412837bdb7acf866da4cc2",
    ),
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def load_public_cr2():
    source = REPOSITORY / "scripts/validate_public_raw.py"
    spec = importlib.util.spec_from_file_location("validate_public_raw", source)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    matches = [row for row in module.FIXTURES if row[3] == "CR2"]
    if len(matches) != 1:
        raise RuntimeError("expected exactly one pinned public CR2 fixture")
    name, url, digest, image_format = matches[0]
    return {
        "name": name,
        "url": url,
        "sha256": digest,
        "format": image_format,
        "license": "CC0-1.0",
        "provenance": "raw.pixls.us repository metadata used by validate_public_raw.py",
    }


def materialize_public_cr2(cache: Path, offline: bool) -> tuple[Path, dict]:
    record = load_public_cr2()
    cache.mkdir(parents=True, exist_ok=True)
    path = cache / record["name"]
    if path.is_file() and sha256(path) == record["sha256"]:
        return path, record
    if offline:
        raise RuntimeError(f"offline and pinned CR2 is absent or invalid: {path}")
    with tempfile.NamedTemporaryFile(dir=cache, delete=False) as temporary:
        candidate = Path(temporary.name)
    try:
        with urllib.request.urlopen(record["url"], timeout=60) as response, candidate.open("wb") as output:
            size = 0
            while True:
                block = response.read(1024 * 1024)
                if not block:
                    break
                size += len(block)
                if size > MAX_RAW_BYTES:
                    raise RuntimeError("public CR2 exceeds bounded fixture download size")
                output.write(block)
        if sha256(candidate) != record["sha256"]:
            raise RuntimeError("downloaded public CR2 checksum mismatch")
        candidate.replace(path)
    finally:
        candidate.unlink(missing_ok=True)
    return path, record


def copy_pinned(name: str, destination: Path) -> dict:
    source, expected = TRACKED_FIXTURES[name]
    actual = sha256(source)
    if actual != expected:
        raise RuntimeError(f"tracked fixture checksum changed: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    return {
        "source": str(source.relative_to(REPOSITORY)),
        "sha256": expected,
        "license": "CC0-1.0",
        "provenance": "repository-owned mathematical fixture; see tests/fixtures/README.md",
    }


def png_chunk(kind: bytes, payload: bytes) -> bytes:
    crc = binascii.crc32(kind)
    crc = binascii.crc32(payload, crc)
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", crc & 0xFFFFFFFF)


def write_reference_png(path: Path) -> None:
    width, height = 64, 48
    rows = []
    for y in range(height):
        row = bytearray([0])
        for x in range(width):
            if x < width // 2 and y < height // 2:
                rgba = (230, 40, 30, 255)
            elif x >= width // 2 and y < height // 2:
                rgba = (30, 200, 70, 192)
            elif x < width // 2:
                rgba = (30, 70, 230, 128)
            else:
                rgba = (235, 220, 45, 0 if x > 56 and y > 40 else 255)
            if x < 6 and y < 14:  # Asymmetric orientation marker.
                rgba = (255, 255, 255, 255)
            row.extend(rgba)
        rows.append(bytes(row))
    payload = b"".join(rows)
    data = b"\x89PNG\r\n\x1a\n"
    data += png_chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    data += png_chunk(b"IDAT", zlib.compress(payload, 9))
    data += png_chunk(b"IEND", b"")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def ffmpeg_version(ffmpeg: str) -> str:
    completed = subprocess.run(
        [ffmpeg, "-version"], check=True, capture_output=True, text=True, timeout=30
    )
    return completed.stdout.splitlines()[0]


def cwebp_version(cwebp: str) -> str:
    completed = subprocess.run(
        [cwebp, "-version"], check=True, capture_output=True, text=True, timeout=30
    )
    return completed.stdout.splitlines()[0]


def encode_raster(ffmpeg: str, source: Path, destination: Path, codec_args: list[str]) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            ffmpeg,
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-threads",
            "1",
            "-i",
            str(source),
            "-map_metadata",
            "-1",
            "-frames:v",
            "1",
            *codec_args,
            str(destination),
        ],
        check=True,
        timeout=60,
    )


def encode_webp(cwebp: str, source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [cwebp, "-quiet", "-lossless", "-exact", "-metadata", "none", str(source), "-o", str(destination)],
        check=True,
        timeout=60,
    )


def xmp_packet(title: str, rating: int, opaque: str) -> str:
    return f"""<?xpacket begin='\ufeff'?>
<x:xmpmeta xmlns:x='adobe:ns:meta/'>
 <rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>
  <rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/' xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmlns:fixture='https://example.invalid/lensworks/fixture/1.0/' xmp:Rating='{rating}' fixture:opaqueProperty='{opaque}'>
   <dc:title><rdf:Alt><rdf:li xml:lang='x-default'>{title}</rdf:li><rdf:li xml:lang='fr'>Titre synthetique</rdf:li></rdf:Alt></dc:title>
   <fixture:UnknownArray><rdf:Bag><rdf:li>one</rdf:li><rdf:li>two</rdf:li></rdf:Bag></fixture:UnknownArray>
   <fixture:Nested rdf:parseType='Resource'><fixture:Value>retained verbatim</fixture:Value></fixture:Nested>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end='w'?>
"""


def adobe_packet(packet: str) -> bytes:
    raw = packet.encode("utf-8")
    return len(raw).to_bytes(4, "big") + zlib.compress(raw)


SCHEMA = """
CREATE TABLE Adobe_variablesTable(id_local INTEGER PRIMARY KEY,name TEXT,value TEXT);
CREATE TABLE AgLibraryRootFolder(id_local INTEGER PRIMARY KEY,id_global TEXT,absolutePath TEXT);
CREATE TABLE AgLibraryFolder(id_local INTEGER PRIMARY KEY,id_global TEXT,rootFolder INTEGER,pathFromRoot TEXT,parentId INTEGER);
CREATE TABLE AgLibraryFile(id_local INTEGER PRIMARY KEY,id_global TEXT,folder INTEGER,baseName TEXT,extension TEXT,idx_filename TEXT);
CREATE TABLE Adobe_images(id_local INTEGER PRIMARY KEY,id_global TEXT,rootFile INTEGER,masterImage INTEGER,copyName TEXT,rating INTEGER,pick INTEGER,colorLabels TEXT,touchTime REAL);
CREATE TABLE Adobe_imageDevelopSettings(id_local INTEGER PRIMARY KEY,image INTEGER,hasBigData INTEGER,text TEXT);
CREATE TABLE Adobe_libraryImageDevelopHistoryStep(id_local INTEGER PRIMARY KEY,id_global TEXT,image INTEGER,dateCreated REAL,text TEXT);
CREATE TABLE Adobe_libraryImageDevelopSnapshot(id_local INTEGER PRIMARY KEY,id_global TEXT,image INTEGER,text TEXT);
CREATE TABLE Adobe_AdditionalMetadata(id_local INTEGER PRIMARY KEY,image INTEGER,xmp BLOB);
CREATE TABLE AgLibraryKeyword(id_local INTEGER PRIMARY KEY,id_global TEXT,name TEXT,parent INTEGER);
CREATE TABLE AgLibraryKeywordImage(id_local INTEGER PRIMARY KEY,image INTEGER,tag INTEGER);
CREATE TABLE AgLibraryCollection(id_local INTEGER PRIMARY KEY,name TEXT,parent INTEGER);
CREATE TABLE AgLibraryCollectionImage(id_local INTEGER PRIMARY KEY,image INTEGER,collection INTEGER,positionInCollection TEXT);
CREATE TABLE Opaque(k INTEGER PRIMARY KEY,n,b,t,r);
CREATE VIEW Unexecuted AS SELECT load_extension('never-run');
"""


def create_lightroom_catalog(
    path: Path,
    originals: Path,
    family: str,
    files: list[tuple[str, str]],
    virtual_file_index: int | None,
    keyword_count: int,
    collection_count: int,
    xmp_file_indices: list[int],
    touch: float,
) -> dict:
    path.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(str(path)) as db:
        db.executescript(SCHEMA)
        db.execute("PRAGMA user_version=13")
        db.executemany(
            "INSERT INTO Adobe_variablesTable VALUES(?,?,?)",
            [
                (1, "Adobe_storeProviderID", f"synthetic-{family}"),
                (2, "Adobe_DBVersion", "1300000"),
            ],
        )
        root_text = str(originals.resolve()) + os.sep
        db.execute("INSERT INTO AgLibraryRootFolder VALUES(1,?,?)", (f"root-{family}", root_text))
        folders: dict[str, int] = {}
        for relative, _ in files:
            folder = str(Path(relative).parent).replace("\\", "/") + "/"
            if folder not in folders:
                folders[folder] = len(folders) + 1
                db.execute(
                    "INSERT INTO AgLibraryFolder VALUES(?,?,?,?,NULL)",
                    (folders[folder], f"folder-{family}-{folders[folder]}", 1, folder),
                )
        image_count = 0
        for index, (relative, stable) in enumerate(files, 1):
            filename = Path(relative).name
            extension = Path(filename).suffix.lstrip(".")
            base = Path(filename).stem
            folder = str(Path(relative).parent).replace("\\", "/") + "/"
            db.execute(
                "INSERT INTO AgLibraryFile VALUES(?,?,?,?,?,?)",
                (index, f"file-{family}-{stable}", folders[folder], base, extension, filename),
            )
            image_count += 1
            db.execute(
                "INSERT INTO Adobe_images VALUES(?,?,?,?,?,?,?,?,?)",
                (image_count, f"image-{family}-{stable}", index, None, None, 3, 1, "blue", touch),
            )
            db.execute(
                "INSERT INTO Adobe_imageDevelopSettings VALUES(?,?,?,?)",
                (image_count, image_count, 1, "opaque Adobe develop settings; retain, never execute"),
            )
            if index in xmp_file_indices:
                packet = xmp_packet(f"Catalog {family} title {index}", 5 - index, f"catalog-{family}-{index}")
                db.execute(
                    "INSERT INTO Adobe_AdditionalMetadata VALUES(?,?,?)",
                    (index, image_count, adobe_packet(packet)),
                )
        virtual_copies = 0
        if virtual_file_index is not None:
            image_count += 1
            virtual_copies = 1
            master = virtual_file_index
            db.execute(
                "INSERT INTO Adobe_images VALUES(?,?,?,?,?,?,?,?,?)",
                (image_count, f"copy-{family}", virtual_file_index, master, f"{family} virtual copy", 2, 0, "", touch + 1),
            )
            db.execute(
                "INSERT INTO Adobe_imageDevelopSettings VALUES(?,?,?,?)",
                (image_count, image_count, 1, "synthetic virtual-copy settings"),
            )
        for index in range(1, keyword_count + 1):
            parent = index - 1 if index > 1 else None
            db.execute(
                "INSERT INTO AgLibraryKeyword VALUES(?,?,?,?)",
                (index, f"keyword-{family}-{index}", f"keyword-{family}-{index}", parent),
            )
            image = 1 + ((index - 1) % image_count)
            db.execute("INSERT INTO AgLibraryKeywordImage VALUES(?,?,?)", (index, image, index))
        membership = 0
        for index in range(1, collection_count + 1):
            db.execute(
                "INSERT INTO AgLibraryCollection VALUES(?,?,?)",
                (index, f"collection-{family}-{index}", None),
            )
            for image in range(1, min(image_count, 2) + 1):
                membership += 1
                db.execute(
                    "INSERT INTO AgLibraryCollectionImage VALUES(?,?,?,?)",
                    (membership, image, index, f"{index:02d}-{image:02d}"),
                )
        db.execute(
            "INSERT INTO Adobe_libraryImageDevelopHistoryStep VALUES(1,?,?,?,?)",
            (f"history-{family}", 1, touch, "opaque history retained only"),
        )
        db.execute(
            "INSERT INTO Adobe_libraryImageDevelopSnapshot VALUES(1,?,?,?)",
            (f"snapshot-{family}", 1, "opaque snapshot retained only"),
        )
        db.execute("INSERT INTO Opaque VALUES(1,NULL,x'00ff01',CAST(x'fffe00' AS TEXT),1.25)")
        db.commit()
        db.execute("VACUUM")
    auxiliary = Path(str(path) + "-data")
    auxiliary.mkdir()
    (auxiliary / "opaque.acr").write_bytes(f"synthetic auxiliary evidence for {family}\n".encode())
    return {
        "files": len(files),
        "images": image_count,
        "masters": len(files),
        "virtual_copies": virtual_copies,
        "keywords": keyword_count,
        "keyword_memberships": keyword_count,
        "collections": collection_count,
        "collection_memberships": collection_count * min(image_count, 2),
        "history_steps": 1,
        "snapshots": 1,
        "catalog_xmp_packets": len(xmp_file_indices),
        "opaque_rows": 1,
    }


def set_tree_mtime(path: Path, timestamp: int) -> None:
    for item in sorted(path.rglob("*"), reverse=True):
        os.utime(item, (timestamp, timestamp))
    os.utime(path, (timestamp, timestamp))


def ensure_destination(destination: Path, replace: bool) -> None:
    if not destination.exists():
        destination.mkdir(parents=True)
        return
    entries = list(destination.iterdir())
    if not entries:
        return
    marker = destination / PACK_MARKER
    if not replace:
        raise RuntimeError(f"destination is not empty; pass --replace for an owned pack: {destination}")
    if not marker.is_file() or marker.read_text().strip() != str(PACK_VERSION):
        raise RuntimeError("refusing to replace a directory without the validation-pack marker")
    shutil.rmtree(destination)
    destination.mkdir(parents=True)


def file_records(root: Path, roles: dict[str, dict]) -> list[dict]:
    result = []
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        relative = path.relative_to(root).as_posix()
        if relative in {"fixture-manifest.json", "fixture-manifest.sha256"}:
            continue
        role = roles.get(relative, {})
        result.append(
            {
                "path": relative,
                "bytes": path.stat().st_size,
                "sha256": sha256(path),
                **role,
            }
        )
    return result


def git_head() -> str:
    return subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=REPOSITORY, check=True, capture_output=True, text=True
    ).stdout.strip()


def build(args) -> Path:
    destination = args.destination.resolve()
    ensure_destination(destination, args.replace)
    marker = destination / PACK_MARKER
    marker.write_text(f"{PACK_VERSION}\n")
    roles: dict[str, dict] = {
        PACK_MARKER: {"role": "ownership marker", "license": "CC0-1.0"},
    }

    originals = destination / "originals"
    date_2014 = originals / "2014/January/2014-01-02"
    date_2015 = originals / "2015/February/2015-02-03"
    date_2014.mkdir(parents=True)
    date_2015.mkdir(parents=True)

    raw, raw_record = materialize_public_cr2(args.raw_cache.resolve(), args.offline)
    cr2 = date_2014 / "canon-6d.CR2"
    shutil.copyfile(raw, cr2)
    roles[cr2.relative_to(destination).as_posix()] = {
        "role": "public real-camera RAW",
        "format": "CR2",
        **raw_record,
    }

    dng = date_2014 / "generated-linear-mask.DNG"
    dng_record = copy_pinned("generated-linear-mask.dng", dng)
    roles[dng.relative_to(destination).as_posix()] = {
        "role": "generated mathematical RAW/profile fixture",
        "format": "DNG",
        **dng_record,
    }

    png = date_2014 / "raster-alpha.png"
    write_reference_png(png)
    roles[png.relative_to(destination).as_posix()] = {
        "role": "generated asymmetric RGBA swatch",
        "format": "PNG",
        "license": "CC0-1.0",
    }
    jpeg = date_2014 / "raster-reference.jpg"
    webp = date_2015 / "raster-lossless.webp"
    bmp = date_2015 / "raster-reference.bmp"
    tiff = date_2015 / "raster-reference.tiff"
    encode_raster(args.ffmpeg, png, jpeg, ["-c:v", "mjpeg", "-q:v", "2", "-pix_fmt", "yuvj444p"])
    encode_webp(args.cwebp, png, webp)
    encode_raster(args.ffmpeg, png, bmp, ["-c:v", "bmp", "-pix_fmt", "bgra"])
    encode_raster(args.ffmpeg, png, tiff, ["-c:v", "tiff", "-compression_algo", "raw", "-pix_fmt", "rgba"])
    for path, image_format, role in [
        (jpeg, "JPEG", "generated lossy raster"),
        (webp, "WebP", "generated lossless alpha raster"),
        (bmp, "BMP", "generated uncompressed alpha raster"),
        (tiff, "TIFF", "generated uncompressed alpha raster"),
    ]:
        roles[path.relative_to(destination).as_posix()] = {
            "role": role,
            "format": image_format,
            "license": "CC0-1.0",
            "generated_from": png.relative_to(destination).as_posix(),
        }
    avif = date_2015 / "raster-10bit.avif"
    avif_record = copy_pinned("generated-swatches-10bit.avif", avif)
    roles[avif.relative_to(destination).as_posix()] = {
        "role": "generated 10-bit color swatch",
        "format": "AVIF",
        **avif_record,
    }

    sidecars = [
        (cr2.with_suffix(".xmp"), xmp_packet("Sidecar Canon title", 2, "sidecar-canon")),
        (dng.with_suffix(".xmp"), xmp_packet("Sidecar DNG title", 1, "sidecar-dng")),
    ]
    for path, packet in sidecars:
        path.write_text(packet, encoding="utf-8")
        roles[path.relative_to(destination).as_posix()] = {
            "role": "generated XMP conflict and unknown-property sidecar",
            "format": "XMP",
            "license": "CC0-1.0",
        }

    relative = {
        "cr2": cr2.relative_to(originals).as_posix(),
        "dng": dng.relative_to(originals).as_posix(),
        "jpeg": jpeg.relative_to(originals).as_posix(),
        "png": png.relative_to(originals).as_posix(),
        "avif": avif.relative_to(originals).as_posix(),
        "webp": webp.relative_to(originals).as_posix(),
        "bmp": bmp.relative_to(originals).as_posix(),
        "tiff": tiff.relative_to(originals).as_posix(),
    }
    catalogs = destination / "lightroom-catalogs"
    current_2014 = catalogs / "2014-v13-2.lrcat"
    current_2015 = catalogs / "2015-v13.lrcat"
    counts_2014 = create_lightroom_catalog(
        current_2014,
        originals,
        "2014-current",
        [(relative[key], key) for key in ["cr2", "dng", "jpeg", "png"]],
        1,
        2,
        2,
        [1, 2],
        1_500_000_000.0,
    )
    counts_2015 = create_lightroom_catalog(
        current_2015,
        originals,
        "2015-current",
        [(relative[key], key) for key in ["cr2", "avif", "webp", "bmp", "tiff"]],
        5,
        3,
        3,
        [1, 5],
        1_600_000_000.0,
    )
    old_2014 = catalogs / "2014-v13.lrcat"
    old_2015 = catalogs / "2015-v13-3.lrcat"
    old_counts_2014 = create_lightroom_catalog(
        old_2014,
        originals,
        "2014-excluded",
        [("2014/January/2014-01-02/excluded-only-2014.jpg", "excluded-2014")],
        None,
        1,
        1,
        [],
        1_400_000_000.0,
    )
    old_counts_2015 = create_lightroom_catalog(
        old_2015,
        originals,
        "2015-excluded",
        [("2015/February/2015-02-03/excluded-only-2015.jpg", "excluded-2015")],
        None,
        1,
        1,
        [],
        1_450_000_000.0,
    )
    mtimes = {
        old_2014: 1_400_000_000,
        current_2014: 1_500_000_000,
        old_2015: 1_450_000_000,
        current_2015: 1_600_000_000,
    }
    for path, timestamp in mtimes.items():
        set_tree_mtime(path, timestamp)
        set_tree_mtime(Path(str(path) + "-data"), timestamp)
        roles[path.relative_to(destination).as_posix()] = {
            "role": "synthetic Lightroom catalog",
            "format": "SQLite lrcat",
            "license": "CC0-1.0",
        }
        auxiliary = Path(str(path) + "-data") / "opaque.acr"
        roles[auxiliary.relative_to(destination).as_posix()] = {
            "role": "synthetic retained Lightroom auxiliary evidence",
            "format": "opaque ACR companion",
            "license": "CC0-1.0",
        }

    expected = {
        "version": PACK_VERSION,
        "direct_import": {
            "unique_assets": 8,
            "master_variants": 8,
            "sidecar_xmp_packets": 2,
            "formats": {name: 1 for name in ["CR2", "DNG", "JPEG", "PNG", "AVIF", "WebP", "BMP", "TIFF"]},
        },
        "lightroom_selection": {
            "families": 2,
            "candidates": 4,
            "selected": ["lightroom-catalogs/2014-v13-2.lrcat", "lightroom-catalogs/2015-v13.lrcat"],
            "excluded": ["lightroom-catalogs/2014-v13.lrcat", "lightroom-catalogs/2015-v13-3.lrcat"],
            "selection_note": "mtime intentionally proves that a numeric suffix is not uniformly current",
        },
        "selected_source_totals": {
            key: counts_2014[key] + counts_2015[key] for key in counts_2014
        },
        "excluded_source_totals": {
            key: old_counts_2014[key] + old_counts_2015[key] for key in old_counts_2014
        },
        "post_migration": {
            "unique_assets_after_shared_original_reconciliation": 8,
            "source_file_rows_accounted": 9,
            "source_variants_accounted": 11,
            "shared_original_paths": 1,
            "catalog_xmp_packets_retained": 4,
            "sidecar_xmp_packets_retained": 2,
            "logical_xmp_conflict_groups": 2,
            "excluded_catalog_file_rows_imported": 0,
        },
    }
    expected_path = destination / "expected-reconciliation.json"
    expected_path.write_text(json.dumps(expected, indent=2, sort_keys=True) + "\n")
    roles[expected_path.relative_to(destination).as_posix()] = {
        "role": "machine-readable expected counts and reconciliation",
        "license": "CC0-1.0",
    }

    for directory in [
        destination / "destinations/export",
        destination / "destinations/backup",
        destination / "destinations/restore",
        destination / "destinations/relink",
        destination / "recovery/unwritable-destination",
    ]:
        directory.mkdir(parents=True)
    sentinel = destination / "recovery/existing-export.jpg"
    sentinel.write_bytes(b"fixture no-clobber sentinel\n")
    roles[sentinel.relative_to(destination).as_posix()] = {
        "role": "no-clobber recovery sentinel; not an image",
        "license": "CC0-1.0",
    }
    readme = destination / "README.md"
    shutil.copyfile(REPOSITORY / "docs/VALIDATION_FIXTURE_ITINERARY.md", readme)
    roles[readme.relative_to(destination).as_posix()] = {
        "role": "portable installed-workflow itinerary",
        "license": "CC0-1.0",
    }

    records = file_records(destination, roles)
    manifest = {
        "format_version": PACK_VERSION,
        "pack_id": "lensworks-cross-platform-disposable-v1",
        "builder_source_commit": git_head(),
        "builder": "scripts/build_validation_fixture_pack.py",
        "scope": "disposable generated/public workflow inputs; not camera or color-quality acceptance",
        "public_fixture_provenance": raw_record,
        "generation": {
            "ffmpeg": ffmpeg_version(args.ffmpeg),
            "cwebp": cwebp_version(args.cwebp),
            "python": os.sys.version.split()[0],
            "absolute_paths_are_generated_for_this_pack": True,
        },
        "counts": {
            "original_assets": 8,
            "xmp_sidecars": 2,
            "lightroom_candidates": 4,
            "selected_lightroom_catalogs": 2,
            "source_files_hashed": len(records),
        },
        "expected_reconciliation": expected,
        "files": records,
        "readiness": {
            "fixture_pack_built": True,
            "all_source_sha256_recorded": True,
            "public_cr2_checksum_verified": True,
            "final_installer_supplied": False,
            "installer_sha256_verified": False,
            "installed_native_workflows_completed": False,
            "windows_sc_23641_complete": False,
            "linux_sc_23642_complete": False,
            "ready_for_release_handoff": False,
        },
    }
    manifest_path = destination / "fixture-manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    (destination / "fixture-manifest.sha256").write_text(
        f"{sha256(manifest_path)}  fixture-manifest.json\n"
    )
    verify(destination)
    return manifest_path


def verify(destination: Path) -> dict:
    manifest_path = destination / "fixture-manifest.json"
    manifest = json.loads(manifest_path.read_text())
    if manifest["format_version"] != PACK_VERSION:
        raise RuntimeError("unsupported validation fixture manifest version")
    seen = set()
    for record in manifest["files"]:
        relative = record["path"]
        if relative in seen:
            raise RuntimeError(f"duplicate manifest path: {relative}")
        seen.add(relative)
        path = destination / relative
        if not path.is_file() or path.stat().st_size != record["bytes"] or sha256(path) != record["sha256"]:
            raise RuntimeError(f"fixture checksum/size mismatch: {relative}")
    actual = {
        path.relative_to(destination).as_posix()
        for path in destination.rglob("*")
        if path.is_file()
        and path.name not in {"fixture-manifest.json", "fixture-manifest.sha256"}
    }
    if actual != seen:
        raise RuntimeError(
            f"fixture manifest file set differs: missing={sorted(seen - actual)} extra={sorted(actual - seen)}"
        )
    if manifest["counts"]["source_files_hashed"] != len(seen):
        raise RuntimeError("fixture manifest source-file count mismatch")
    expected = json.loads((destination / "expected-reconciliation.json").read_text())
    if expected != manifest["expected_reconciliation"]:
        raise RuntimeError("expected reconciliation differs from manifest")
    formats = expected["direct_import"]["formats"]
    observed = {key: 0 for key in formats}
    for record in manifest["files"]:
        image_format = record.get("format")
        if image_format in observed:
            observed[image_format] += 1
    if observed != formats:
        raise RuntimeError(f"format counts differ: {observed}")
    cr2 = [record for record in manifest["files"] if record.get("format") == "CR2"]
    if len(cr2) != 1 or cr2[0]["sha256"] != manifest["public_fixture_provenance"]["sha256"]:
        raise RuntimeError("public CR2 provenance/checksum differs from file manifest")
    checksum_line = (destination / "fixture-manifest.sha256").read_text().split()
    if checksum_line != [sha256(manifest_path), "fixture-manifest.json"]:
        raise RuntimeError("fixture manifest checksum mismatch")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--destination",
        type=Path,
        default=REPOSITORY / ".deps/validation-fixtures/lensworks-cross-platform-v1",
    )
    parser.add_argument("--raw-cache", type=Path, default=REPOSITORY / ".deps/raw-fixtures")
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--cwebp", default="cwebp")
    parser.add_argument("--offline", action="store_true", help="refuse network download of missing CR2")
    parser.add_argument("--replace", action="store_true", help="replace only a marker-owned existing pack")
    parser.add_argument("--verify-only", action="store_true")
    args = parser.parse_args()
    if args.verify_only:
        manifest = verify(args.destination.resolve())
        print(json.dumps({"verified": True, "pack_id": manifest["pack_id"], "files": len(manifest["files"])}))
    else:
        manifest = build(args)
        print(manifest)


if __name__ == "__main__":
    main()
