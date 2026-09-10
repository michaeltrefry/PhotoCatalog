#!/usr/bin/env python3
"""Prospective S8 qualification. Planning is read-only except exclusive receipts.

This checkpoint deliberately has no execution command: final source, independent
oracles, service phase metrics and process watchdog must be reviewed before run
admission is implemented. A plan is neither execution authorization nor a pass.
"""
from __future__ import annotations
import argparse
import copy
import hashlib
import json
from pathlib import Path

MIB = 1024**2
GIB = 1024**3
IDS = (
    "private-Canon-EOS-6D-PSD", "private-Canon-EOS-6D-RAW", "private-Canon-EOS-6D-TIFF",
    "private-Canon-EOS-REBEL-T3-DNG", "private-Canon-EOS-REBEL-T3-JPG", "private-Canon-EOS-REBEL-T3-RAW",
    "private-Canon-PowerShot-G15-JPG", "private-Canon-PowerShot-G15-RAW", "private-DMC-TS3-JPG",
    "private-DMC-ZS19-JPG", "private-Canon-EOS-5D-Mark-IV-DNG", "private-Canon-EOS-5D-Mark-IV-JPG",
    "private-Canon-EOS-5D-Mark-IV-RAW", "private-Canon-EOS-60D-DNG", "private-Canon-EOS-7D-DNG",
    "private-Canon-EOS-7D-RAW", "private-FC1102-JPG", "private-L1D-20c-DNG", "private-L1D-20c-JPG",
    "private-X-T3-DNG", "private-X-T3-JPG", "private-X-T3-RAW", "public-canon-6d",
    "public-panasonic-gx7mk2", "prep-transfer", "prep-alpha", "prep-orientation",
    "prep-quantization", "prep-gradient", "prep-noise",
)
TIMING = (
    "private-Canon-EOS-5D-Mark-IV-RAW", "private-X-T3-RAW", "private-X-T3-DNG",
    "private-L1D-20c-DNG", "public-panasonic-gx7mk2", "private-Canon-EOS-6D-PSD",
    "private-Canon-EOS-6D-TIFF", "private-Canon-PowerShot-G15-JPG",
)
RAW_TIMING = TIMING[:5]
EXPORT_TIMING = (TIMING[0], TIMING[2], TIMING[-1])

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()

def sha(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()

def recipe(**changes):
    settings = dict(crop=None, straighten_degrees=0.0, exposure_ev=0.0,
                    white_balance={"mode": "as_shot"}, contrast=0.0, highlights=0.0,
                    shadows=0.0, saturation=0.0, vibrance=0.0,
                    sharpening={"amount": 0.0, "radius_px": 1.0},
                    noise_reduction={"luminance": 0.0, "chroma": 0.0})
    settings.update(changes)
    return {"version": "1", "settings": settings}

def recipes():
    # Pair order is fixed. CPU kernel trials use A. Service trials alternate A/B
    # so catalog identity advances and both WB bases can be warmed explicitly.
    return {
        "neutral": [recipe(), recipe(exposure_ev=0.01)],
        "crop": [recipe(crop=dict(left=.05, top=.05, right=.95, bottom=.95)),
                 recipe(crop=dict(left=.04, top=.04, right=.96, bottom=.96))],
        "straighten": [recipe(straighten_degrees=2.0), recipe(straighten_degrees=-2.0)],
        "exposure": [recipe(exposure_ev=.75), recipe(exposure_ev=-.75)],
        "white_balance": [recipe(white_balance=dict(mode="temperature_tint", kelvin=4300, tint=5.0)),
                          recipe(white_balance=dict(mode="temperature_tint", kelvin=6500, tint=-5.0))],
        "contrast": [recipe(contrast=.25), recipe(contrast=-.25)],
        "highlights": [recipe(highlights=-.3), recipe(highlights=.3)],
        "shadows": [recipe(shadows=.3), recipe(shadows=-.3)],
        "saturation": [recipe(saturation=.2), recipe(saturation=-.2)],
        "vibrance": [recipe(vibrance=.2), recipe(vibrance=-.2)],
        "sharpening": [recipe(sharpening=dict(amount=.5, radius_px=1.25)),
                       recipe(sharpening=dict(amount=.25, radius_px=1.25))],
        "luminance_noise": [recipe(noise_reduction=dict(luminance=.25, chroma=0.0)),
                            recipe(noise_reduction=dict(luminance=.5, chroma=0.0))],
        "chroma_noise": [recipe(noise_reduction=dict(luminance=0.0, chroma=.25)),
                         recipe(noise_reduction=dict(luminance=0.0, chroma=.5))],
        "combined": [recipe(crop=dict(left=.02, top=.03, right=.98, bottom=.97),
                            straighten_degrees=1.5, exposure_ev=.5,
                            white_balance=dict(mode="temperature_tint", kelvin=4300, tint=5.0),
                            contrast=.2, highlights=-.3, shadows=.25, saturation=.1, vibrance=.15,
                            sharpening=dict(amount=.4, radius_px=1.25),
                            noise_reduction=dict(luminance=.15, chroma=.2)),
                     recipe(crop=dict(left=.02, top=.03, right=.98, bottom=.97),
                            straighten_degrees=1.5, exposure_ev=.6,
                            white_balance=dict(mode="temperature_tint", kelvin=4300, tint=5.0),
                            contrast=.2, highlights=-.3, shadows=.25, saturation=.1, vibrance=.15,
                            sharpening=dict(amount=.4, radius_px=1.25),
                            noise_reduction=dict(luminance=.15, chroma=.2))],
    }

def outputs():
    result = {}
    for name, fmt in [
        ("jpeg8", dict(format="jpeg", quality=90)),
        ("png8", dict(format="png", depth="eight")),
        ("png16", dict(format="png", depth="sixteen")),
        ("tiff8", dict(format="tiff", depth="eight")),
        ("tiff16", dict(format="tiff", depth="sixteen")),
        ("tiff32", dict(format="tiff", depth="float32")),
    ]:
        result[name] = dict(size={"mode": "original"}, format=fmt,
                            profile={"kind": "linear_srgb" if name == "tiff32" else "srgb"},
                            alpha={"mode": "composite", "linear_rgb": [1.0, 1.0, 1.0]}
                            if name == "jpeg8" else {"mode": "preserve"})
    return result

def validate_manifest(manifest):
    values = manifest.get("inputs", [])
    if manifest.get("version") != 1 or len(values) != 30 or {i.get("id") for i in values} != set(IDS):
        raise ValueError("exact frozen 30-input cohort required")
    for item in values:
        if not Path(item["path"]).is_absolute() or (len(item["sha256"]) != 64 or any(c not in "0123456789abcdef" for c in item["sha256"])):
            raise ValueError("input path/hash")
        if any(type(item[k]) is not int or item[k] <= 0 for k in ("width", "height")):
            raise ValueError("input dimensions")
        if item["width"]*item["height"] > 32_000_000:
            raise ValueError("timing cohort differs from validated <=32MP inputs")

def plan(manifest):
    validate_manifest(manifest)
    r, o = recipes(), outputs()
    cases = []
    def add(phase, fixture, operation, selected_recipes, selected_outputs, warmups, repeats, deadline):
        cases.append(dict(id=f"{phase}-{fixture}-{operation}", phase=phase, fixture_id=fixture,
                          operation=operation, recipes=copy.deepcopy(selected_recipes),
                          outputs=copy.deepcopy(selected_outputs), warmups=warmups,
                          repetitions=repeats, deadline_seconds=deadline))
    for fixture in IDS:
        # Repeat full pixel observations in independent processes. Export matrix
        # artifacts are retained once; codecs have their separate repeated gate.
        for repetition in (0, 1):
            add("correctness", fixture, f"all-{repetition}", [p[0] for p in r.values()], [], 0, 1, 900)
        add("correctness", fixture, "combined", [r["combined"][0]], list(o.values()), 0, 1, 900)
    for fixture in TIMING:
        for operation, pair in r.items():
            add("kernel", fixture, operation, pair[:1], [], 2, 100, 900)
            if operation != "neutral":
                add("warm_service", fixture, operation, pair, [], 2, 100, 900)
        add("full", fixture, "combined", r["combined"][:1], [], 2, 20, 900)
    for fixture in RAW_TIMING:
        add("first_raw", fixture, "combined", r["combined"], [], 2, 20, 1200)
    for fixture in EXPORT_TIMING:
        for name, output in o.items():
            add("export", fixture, name, r["combined"][:1], [output], 2, 20, 1800)
    return dict(version=1, status="prospective_source_checkpoint_not_admitted",
                inputs=manifest["inputs"], recipes=r, outputs=o, cases=cases,
                normal_limits=dict(decode=dict(max_encoded_bytes=512*MIB,
                    max_intermediate_pixels=32_000_000, max_allocation_bytes=2*GIB),
                    render=dict(max_pixels=32_000_000, max_allocation_bytes=2*GIB, max_live_bytes=4*GIB),
                    encoded_extent=512*MIB, sampled_worker_rss_stop_bytes=4*GIB,
                    sampled_group_rss_stop_bytes=4*GIB+512*MIB, sampled_interval_seconds=.1),
                admission=dict(workers=1, automatic_retries=0, minimum_free_bytes=96*GIB,
                    output_stop_bytes=128*GIB, original_source_writes=False,
                    source_copy="one exclusive byte-verified ordinary copy per input, never hard-link originals"),
                budgets=dict(kernel_p95_ms=100, warm_service_p95_ms=250, full_p95_ms=5000,
                    first_raw_p95_ms=20000, jpeg_p95_ms=25000, png_p95_ms=40000,
                    tiff_p95_ms=30000, durable_edit_during_import_p95_ms=100),
                pending=["independent numerical/color/export verifier and generated analytic fixtures",
                         "custom ICC/metadata/size/alpha matrix requests",
                         "100MP fixture generation and resource-refusal requests",
                         "durable edit acknowledgments under actually overlapping import/export",
                         "owned process watchdog/execution admission after source review",
                         "final source/SDK/binary/config identities and service phase metrics"])

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.manifest.stat().st_size > MIB:
        raise ValueError("manifest byte bound")
    value = plan(json.loads(args.manifest.read_text()))
    value["manifest_sha256"] = sha(args.manifest)
    value["planner_sha256"] = sha(__file__)
    value["matrix_sha256"] = hashlib.sha256(canonical(value["cases"])).hexdigest()
    with args.output.open("x") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")

if __name__ == "__main__":
    main()
