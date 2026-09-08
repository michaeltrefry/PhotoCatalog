#!/usr/bin/env python3
"""Validate a private image corpus without placing images or receipts in Git.

The manifest supplies fixture/source paths, SHA-256, byte size, source mtime,
and optional independently measured `reference` fields from render_probe JSON.
The probe runs in a fresh process twice per file; previews go to a new folder.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


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
        for field in ("source", "fixture"):
            if output.is_relative_to(Path(row[field]).resolve().parent):
                raise ValueError("receipt directory must be separate from source folders")
        verify_source(row)
    output.mkdir(parents=True, exist_ok=False)
    probe = args.probe.resolve()
    receipts = []
    for index, row in enumerate(rows):
        receipt = {"camera": row["camera"], "format": row["format"],
                   "source_sha256": row["sha256"], "passed": False}
        try:
            runs = []
            for repeat in range(2):
                command = [str(probe), row["fixture"]]
                if repeat == 0:
                    command.append(str(output / f"{index:02d}.jpg"))
                result = subprocess.run(command, capture_output=True, text=True, timeout=180)
                value = json.loads(result.stdout)
                runs.append(value)
                if result.returncode or value.get("status") != "decoded":
                    raise ValueError(f"decode failed: {value}")
                if value.get("nonfinite_components") != 0:
                    raise ValueError("nonfinite pixel values or missing finiteness evidence")
                if value["width"] <= 0 or value["height"] <= 0:
                    raise ValueError("empty decoded image")
                alpha_pixels = sum(value[key] for key in ("alpha_zero", "alpha_partial", "alpha_opaque"))
                if alpha_pixels != value["width"] * value["height"]:
                    raise ValueError("alpha coverage does not match image dimensions")
                for key, expected in row.get("reference", {}).items():
                    if value.get(key) != expected:
                        raise ValueError(f"independent reference mismatch: {key}, expected {expected}, got {value.get(key)}")
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
