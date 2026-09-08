#!/usr/bin/env python3
"""Generate mathematical DNG/profile oracles using NumPy and tifffile, never photos."""
import json
import struct
from pathlib import Path

import numpy as np
import tifffile

ROOT = Path(__file__).resolve().parents[1] / "tests/fixtures"
NEUTRAL = np.array([0.5, 1.0, 0.75])
XYZ_TO_SRGB = np.array([
    [3.1338561, -1.6168667, -0.4906146],
    [-0.9787684, 1.9161415, 0.0334540],
    [0.0719453, -0.2289914, 1.4052427],
])
# Independently specified ProPhoto/D50 matrix, not obtained from the SDK.
XYZ_TO_PROPHOTO = np.array([
    [1.3459433, -0.2556075, -0.0511118],
    [-0.5445989, 1.5081673, 0.0205351],
    [0.0, 0.0, 1.2118128],
])


def rational(values):
    result = []
    for value in np.array(values).ravel():
        result.extend([int(round(float(value) * 1_000_000)), 1_000_000])
    return tuple(result)


def generate(name, forward, patches, calibration=False, spatial=0):
    camera_matrix = np.diag(NEUTRAL) @ np.linalg.inv(forward)
    tags = [
        (254, "I", 1, 0, False), (274, "H", 1, 1, False),
        (50706, "B", 4, (1, 4, 0, 0), False),
        (50707, "B", 4, (1, 4, 0, 0), False),
        (50708, "s", 0, "PhotoCatalog mathematical fixture", False),
        (50721, "2i", 9, rational(camera_matrix), False),
        (50964, "2i", 9, rational(forward), False),
        (50778, "H", 1, 23, False),
        (50728, "2I", 3, rational(NEUTRAL), False),
        (50717, "I", 3, (1, 1, 1), False),
        (50714, "2I", 3, (0, 1, 0, 1, 0, 1), False),
        (50713, "H", 2, (1, 1), False),
    ]
    if calibration:
        # Six hue nodes, two saturation nodes, one value plane. Hue shift=0,
        # saturation scale=1/2, value scale=1 at zero saturation and3/4 at full.
        # Analytic result: (rgb/2 + max(rgb)/2) * (1 - saturation/4).
        table = [value for _ in range(6) for entry in [(0, 0.5, 1), (0, 0.5, 0.75)] for value in entry]
        tags += [(50937, "I", 3, (6, 2, 1), False),
                 (50938, "f", len(table), table, False),
                 (51107, "I", 1, 0, False)]
    if spatial:
        tags[2] = (50706, "B", 4, (1, 7, 1, 0), False)
        tags[3] = (50707, "B", 4, (1, 6, 0, 0), False)
        weights = [0, .25, 0, 0, .25]
        gains = [1 + .2*y + .4*x + .6*w for y in range(2) for x in range(2) for w in range(2)]
        blob = struct.pack("<II4dI5f", 2, 2, 1., 1., 0., 0., 2, *weights)
        if spatial == 2:
            blob += struct.pack("<I3f", 3, 2., 1., 3.) # float32 table, gamma 2
        blob += struct.pack("<8f", *gains)
        tags += [(52544 if spatial == 2 else 52525, "B", len(blob), blob, False),
                 (50730, "2i", 1, (1, 1), False)] # baseline exposure +1 stop
    pixels = np.tile(np.asarray(patches, dtype=np.float32), (16, 6, 1))
    mask = np.tile(np.array([0, 128, 255, 255, 255, 128], dtype=np.uint8), (16, 6))
    path = ROOT / f"{name}.dng"
    with tifffile.TiffWriter(path) as writer:
        writer.write(pixels, photometric="rgb", planarconfig="contig", metadata=None,
                     extratags=tags, subifds=1, rowsperstrip=16)
        writer.write(mask, photometric=4, extratags=[(254, "I", 1, 4, False)],
                     metadata=None, rowsperstrip=16)
    # tifffile treats unknown photometric codes as grayscale and invents extra
    # samples; changing this scalar after an ordinary RGB write avoids that.
    with tifffile.TiffFile(path) as source:
        offset = source.pages[0].tags[262].valueoffset
        spatial_type_offset = source.pages[0].tags[52544 if spatial == 2 else 52525].offset + 2 if spatial else None
    with path.open("r+b") as output:
        output.seek(offset)
        output.write((34892).to_bytes(2, "little"))
        if spatial_type_offset:
            output.seek(spatial_type_offset)
            output.write((7).to_bytes(2, "little")) # UNDEFINED tag datatype
    camera = np.asarray(patches, dtype=np.float32).astype(np.float64)
    xyz = (forward @ np.diag(1 / NEUTRAL) @ camera.T).T
    expected = (XYZ_TO_SRGB @ xyz.T).T
    applied = []
    if calibration:
        prophoto = (XYZ_TO_PROPHOTO @ xyz.T).T
        for index, rgb in enumerate(prophoto):
            in_domain = bool(np.all(rgb >= 0) and np.all(rgb <= 1))
            applied.append(in_domain)
            if in_domain:
                maximum, minimum = rgb.max(), rgb.min()
                saturation = (maximum - minimum) / maximum if maximum else 0
                corrected = (rgb * 0.5 + maximum * 0.5) * (1 - saturation * 0.25)
                expected[index] = XYZ_TO_SRGB @ np.linalg.solve(XYZ_TO_PROPHOTO, corrected)
    spatial_expected = []
    if spatial:
        for y in range(16):
            row = []
            for x in range(36):
                rgb = XYZ_TO_PROPHOTO @ np.linalg.solve(XYZ_TO_SRGB, expected[x % 6])
                weight = np.clip(2 * (.25*rgb[1] + .25*rgb.max()), 0, 1)
                if spatial == 2: weight = weight ** 2
                gain = 1 + .2*(y+.5)/16 + .4*(x+.5)/36 + .6*min(2*weight, 1)
                row.append((expected[x % 6]*gain).tolist())
            spatial_expected.append(row)
    receipt = {"spatial_linear_srgb": spatial_expected, "linear_srgb": expected.tolist(), "alpha": [0, 128 / 255, 1, 1, 1, 128 / 255],
               "width": 36, "height": 16, "calibration_applied": applied}
    (ROOT / f"{name}.expected.json").write_text(json.dumps(receipt, indent=2) + "\n")


generate("generated-linear-mask",
         np.array([[0.5, 0.3, 0.1643], [0.2, 0.7, 0.1], [0.05, 0.1, 0.6751]]),
         [NEUTRAL * 0.25, [0.2, 0, 0], [0, 0.2, 0], [0, 0, 0.2], NEUTRAL, [0.5, 0, 0]])
generate("generated-calibration-mask",
         np.array([[0.7, 0.3, -0.0357], [0.15, 0.9, -0.05], [0.03, 0.07, 0.7251]]),
         [NEUTRAL * 0.25, [0.2, 0, 0], [0, 0.2, 0], [0, 0, 0.2], NEUTRAL * 0.75, [0.6, 0, 0]],
         calibration=True)

for spatial in (1, 2):
    generate(f"generated-spatial{spatial}-mask",
             np.array([[0.7, 0.3, -0.0357], [0.15, 0.9, -0.05], [0.03, 0.07, 0.7251]]),
             [NEUTRAL * 0.25, [0.2, 0, 0], [0, 0.2, 0], [0, 0, 0.2], NEUTRAL * 0.75, [.6, 0, 0]],
             calibration=True, spatial=spatial)
