#!/usr/bin/env python3
"""Fetch and verify Adobe's exact SDK archive; vendor sources remain outside Git."""
import argparse
import hashlib
import pathlib
import urllib.request
import zipfile

URL = "https://download.adobe.com/pub/adobe/dng/dng_sdk_1_7_1_2724_20260908.zip"
SHA256 = "740fbe95c69e09e9cd17654a5e4fef2d7021254b06fd2b8c5557b79a1496b50c"

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--destination", type=pathlib.Path, required=True)
    args = parser.parse_args()
    destination = args.destination.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / "dng_sdk_1_7_1_2724.zip"
    if not archive.exists():
        temporary = archive.with_suffix(".download")
        with urllib.request.urlopen(URL, timeout=120) as source, temporary.open("wb") as output:
            while block := source.read(1024 * 1024):
                output.write(block)
        temporary.replace(archive)
    with archive.open("rb") as source:
        digest = hashlib.sha256()
        while block := source.read(1024 * 1024):
            digest.update(block)
        actual = digest.hexdigest()
    if actual != SHA256:
        raise RuntimeError(f"SDK checksum mismatch: {actual}")
    extraction = destination / "dng_sdk_1_7_1_2724"
    extraction.mkdir(exist_ok=True)
    with zipfile.ZipFile(archive) as source:
        for member in source.infolist():
            path = (extraction / member.filename).resolve()
            if not path.is_relative_to(extraction):
                raise RuntimeError("SDK archive path escapes destination")
            if (member.external_attr >> 16) & 0o170000 == 0o120000:
                raise RuntimeError("SDK archive contains a symbolic link")
        source.extractall(extraction)
    root = destination / "dng_sdk_1_7_1_2724" / "dng_sdk_1_7_1"
    if not (root / "dng_sdk/source/dng_negative.cpp").is_file():
        raise RuntimeError(f"unexpected SDK layout: {root}")
    print(root)

if __name__ == "__main__":
    main()
