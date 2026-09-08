#!/usr/bin/env python3
"""Validate real CR2/JPEG fixtures without copying them into Git or changing them."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    args = parser.parse_args()
    binary, fixtures = args.binary.resolve(strict=True), args.fixtures.resolve(strict=True)
    originals = sorted(p for p in fixtures.rglob("*") if p.is_file() and not p.is_symlink())
    if not originals or not any(p.suffix.lower() == ".cr2" for p in originals):
        raise RuntimeError("A real CR2 fixture is required")
    if not any(p.suffix.lower() in {".jpg", ".jpeg"} for p in originals):
        raise RuntimeError("A real JPEG fixture is required")
    before = {p: (p.stat().st_size, p.stat().st_mtime_ns, digest(p)) for p in originals}
    with tempfile.TemporaryDirectory(prefix="photocatalog-private-validation-") as directory:
        root = Path(directory)
        catalog = root / "catalog"

        def run(*command, parse=True):
            result = subprocess.run(
                [str(binary), "--catalog", str(catalog), *map(str, command)],
                check=False, capture_output=True, text=True, timeout=180,
            )
            if result.returncode:
                raise RuntimeError(f"CLI {command[0]} failed ({result.returncode}): {result.stderr}")
            return json.loads(result.stdout) if parse else result

        imported = run("import", fixtures)
        first = run("browse", "--limit", 1)
        if len(first) != 1:
            raise RuntimeError("Expected a first page")
        rest = run("browse", "--after", first[0]["sequence"], "--limit", 1000)
        assets = first + rest
        if len(assets) != imported["imported"] or len(assets) < 2:
            raise RuntimeError("Fixture set exceeds one page or import/page count differs")
        if len({asset["id"] for asset in assets}) != len(assets):
            raise RuntimeError("Duplicate identities across pages")
        evidence = []
        for index, asset in enumerate(assets):
            fetched = run("get", asset["id"])
            if fetched != asset or asset["state"] != "ready":
                raise RuntimeError("Asset did not survive process restart as ready")
            metadata = asset["metadata"]
            if metadata["width"] <= 0 or metadata["height"] <= 0:
                raise RuntimeError("Invalid image dimensions")
            output = root / f"preview-{index}.jpg"
            run("preview", asset["id"], output, parse=False)
            encoded = output.read_bytes()
            if not (encoded.startswith(b"\xff\xd8") and encoded.endswith(b"\xff\xd9")):
                raise RuntimeError("Preview is not a complete JPEG stream")
            evidence.append({"format": metadata["format"], "width": metadata["width"],
                             "height": metadata["height"], "preview_bytes": len(encoded)})
        retry = run("import", fixtures)
        if retry["imported"] != 0 or retry["unchanged"] != len(assets):
            raise RuntimeError("Repeated import did not retain existing records")
        if run("browse", "--limit", 1000) != assets:
            raise RuntimeError("Repeated import changed identities or metadata")
        after = {p: (p.stat().st_size, p.stat().st_mtime_ns, digest(p)) for p in originals}
        if before != after:
            raise RuntimeError("Source fixture contents or modification metadata changed")
        print(json.dumps({"passed": True, "assets": len(assets), "source_files_unchanged": len(originals),
                          "restart_and_pagination": True, "idempotent_retry": True,
                          "previews": evidence}, indent=2))


if __name__ == "__main__":
    main()
