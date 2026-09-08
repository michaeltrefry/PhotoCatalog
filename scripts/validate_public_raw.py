#!/usr/bin/env python3
"""Fetch checksum-pinned CC0 RAW fixtures and optionally validate full decoding.

Source and per-file license/checksum record:
https://raw.pixls.us/json/getrepository.php?set=all
Images remain in the ignored dependency directory, never in Git.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import urllib.request

FIXTURES = [
    ("canon-6d.cr2", "https://raw.pixls.us/getfile.php/799/nice/Canon%20-%20EOS%206D%20-%20RAW.CR2",
     "360842779d08fe4805b4a2fe4979f06fdc6da982c85c3acb181f25bcf4c9f536", "CR2"),
    ("panasonic-gx7mk2.rw2", "https://raw.pixls.us/getfile.php/2774/nice/Panasonic%20-%20DMC-GX7MK2%20-%204:3.RW2",
     "6109abdf7cc633c1e0c9e7ec1a005a396313fe564998c3e2eeca48c8436fed11", "RW2"),
]

# Independently read from these checksum-pinned files: Canon SensorInfo inclusive
# borders and RAW lossless-JPEG SOF3; Panasonic IFD0 exclusive crop and bit depth.
# Camera strings are source IFD0 ASCII, not normalized decoder aliases.
REFERENCES = {
    "canon-6d.cr2": {"width": 5472, "height": 3648, "bits": 14,
                     "make": "Canon", "model": "Canon EOS 6D"},
    "panasonic-gx7mk2.rw2": {"width": 4592, "height": 3448, "bits": 12,
                           "make": "Panasonic", "model": "DMC-GX7MK2"},
}


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, default=Path(".deps/raw-fixtures"))
    parser.add_argument("--probe", type=Path)
    args = parser.parse_args()
    args.destination.mkdir(parents=True, exist_ok=True)
    for name, url, expected, image_format in FIXTURES:
        path = args.destination / name
        if not path.exists() or digest(path) != expected:
            with tempfile.NamedTemporaryFile(dir=args.destination, delete=False) as temporary:
                temporary_path = Path(temporary.name)
                try:
                    with urllib.request.urlopen(url, timeout=60) as response:
                        size = 0
                        for chunk in iter(lambda: response.read(1024 * 1024), b""):
                            size += len(chunk)
                            if size > 64 * 1024 * 1024:
                                raise ValueError("unexpectedly large RAW fixture")
                            temporary.write(chunk)
                    temporary.close()
                    if digest(temporary_path) != expected:
                        raise ValueError("downloaded RAW fixture checksum mismatch")
                    temporary_path.replace(path)
                finally:
                    temporary.close()
                    temporary_path.unlink(missing_ok=True)
        reference = REFERENCES[name]
        receipt = {"fixture": name, "sha256": expected, "license": "CC0-1.0",
                   "source": url, "independent_header_reference": reference}
        if args.probe:
            result = subprocess.run([str(args.probe.resolve()), str(path.resolve())],
                                    capture_output=True, text=True, timeout=180, check=True)
            decoded = json.loads(result.stdout)
            if (decoded.get("status") != "decoded" or decoded.get("nonfinite_components") != 0
                    or decoded["width"] != reference["width"]
                    or decoded["height"] != reference["height"]
                    or decoded["metadata"]["format"] != image_format
                    or decoded["metadata"]["preview_source"] != "full-quality original rendering"
                    or decoded["metadata"]["camera_make"] != reference["make"]
                    or decoded["metadata"]["camera_model"] != reference["model"]
                    or decoded["provenance"]["source_bits_per_channel"] != reference["bits"]):
                raise ValueError(f"full RAW decode contract failed: {decoded}")
            if digest(path) != expected:
                raise ValueError("RAW decoder changed its input")
            receipt["decode"] = decoded
        print(json.dumps(receipt), flush=True)


if __name__ == "__main__":
    main()
