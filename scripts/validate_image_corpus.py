#!/usr/bin/env python3
"""Validate a private image corpus without placing images or receipts in Git.

The manifest supplies fixture/source paths, SHA-256, byte size, source mtime,
and mandatory independent dimensions, precision, format and camera references.
The probe runs in a fresh process twice per file; previews go to a new folder.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def validate_reference(row):
    if not row.get("reference_source"):
        raise ValueError("independent reference source is required")
    if not row.get("expected_format") or row["expected_format"] == "RAW":
        raise ValueError("explicit independently identified format is required")
    reference = row.get("reference", {})
    for key in ("width", "height"):
        if not isinstance(reference.get(key), int) or reference[key] <= 0:
            raise ValueError(f"independent {key} is required")
    bits = row.get("expected_source_bits")
    if not isinstance(bits, int) or not 1 <= bits <= 64:
        raise ValueError("independent source precision is required")
    for key in ("expected_camera_make", "expected_camera_model"):
        if key not in row:
            raise ValueError(f"independent {key} is required (null if absent)")
        if row[key] is None and not row.get("camera_reference_note"):
            raise ValueError("absent camera metadata requires a reference explanation")


def validate_render(row, value):
    if value.get("status") != "decoded":
        raise ValueError(f"decode failed: {value}")
    metadata = value["metadata"]
    if metadata["format"] != row["expected_format"]:
        raise ValueError("decoded format differs from independent reference")
    if metadata.get("preview_source") != "full-quality original rendering":
        raise ValueError("full-quality original provenance is required")
    if value["provenance"]["source_bits_per_channel"] != row["expected_source_bits"]:
        raise ValueError("decoded source precision differs from independent reference")
    for key in ("camera_make", "camera_model"):
        if metadata.get(key) != row[f"expected_{key}"]:
            raise ValueError(f"decoded {key} differs from independent reference")
    if value.get("nonfinite_components") != 0:
        raise ValueError("nonfinite pixel values or missing finiteness evidence")
    alpha_pixels = sum(value[key] for key in ("alpha_zero", "alpha_partial", "alpha_opaque"))
    if alpha_pixels != value["width"] * value["height"]:
        raise ValueError("alpha coverage does not match image dimensions")
    for key, expected in row["reference"].items():
        if value.get(key) != expected:
            raise ValueError(f"independent reference mismatch: {key}, expected {expected}, got {value.get(key)}")


def verify_source(row):
    for field in ("source", "fixture"):
        path = Path(row[field])
        with path.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if digest != row["sha256"] or path.stat().st_size != row["bytes"]:
            raise ValueError(f"{field} content changed for {row['camera']}")
    if Path(row["source"]).stat().st_mtime_ns != row["source_mtime_ns"]:
        raise ValueError(f"source modification time changed for {row['camera']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    rows = json.loads(args.manifest.read_text())
    if not rows:
        raise ValueError("empty corpus")
    output = args.output.resolve()
    for row in rows:
        validate_reference(row)
        for field in ("source", "fixture"):
            if output.is_relative_to(Path(row[field]).resolve().parent):
                raise ValueError("receipt directory must be separate from source folders")
        verify_source(row)
    output.mkdir(parents=True, exist_ok=False)
    probe = args.probe.resolve()
    receipts = []
    for index, row in enumerate(rows):
        receipt = {"camera": row["camera"], "format": row["format"],
                   "source_sha256": row["sha256"], "passed": False,
                   "independent_reference": {key: row[key] for key in row
                                             if key.startswith("expected_") or key in
                                             ("reference", "reference_source", "camera_reference_note")}}
        try:
            runs = []
            for repeat in range(2):
                command = [str(probe), row["fixture"]]
                if repeat == 0:
                    command.append(str(output / f"{index:02d}.jpg"))
                result = subprocess.run(command, capture_output=True, text=True, timeout=180)
                value = json.loads(result.stdout)
                runs.append(value)
                if result.returncode:
                    raise ValueError(f"decode failed: {value}")
                validate_render(row, value)
            if runs[0]["pixel_blake3"] != runs[1]["pixel_blake3"]:
                raise ValueError("pixels differ between fresh-process renders")
            receipt["runs"] = runs
            receipt["passed"] = True
        except (ValueError, KeyError, subprocess.TimeoutExpired) as error:
            receipt["error"] = str(error)
        finally:
            verify_source(row)
            receipts.append(receipt)
            (output / "receipt.json").write_text(json.dumps(receipts, indent=2) + "\n")
        print(json.dumps({"camera": row["camera"], "format": row["format"], "passed": receipt["passed"]}), flush=True)
    if not all(row["passed"] for row in receipts):
        raise SystemExit("Corpus validation failed; inspect private receipt.json")
    print(json.dumps({"passed": len(receipts), "deterministic": True, "sources_unchanged": True}))


if __name__ == "__main__":
    main()
