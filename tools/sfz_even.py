"""Even out the per-note level of a simple one-sample-per-key-range SFZ (like the VCSL maps).

    python -I tools/sfz_even.py SRC.sfz OUT.sfz [--release S] [--vels 50,120] [--window 0.6]

Each region's key center is rendered at each velocity in --vels (one per velocity layer, low to high: the
VCSL crossfaded maps hold layer 1 alone below xfout_lovel and the top layer alone above xfin_hivel), the
RMS of the first --window seconds is measured, and the region's volume= is moved toward the median of its
layer (clamped to +-12 dB). The output's default_path points (relatively) back at the source folder, so no
samples move.
"""
import argparse
import os
import re
import sys

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
from sfizz_engine import SfizzSynth  # noqa: E402

SR = 44100


def level(syn, key, vel, window):
    y = syn.render([(10, "on", key, vel), (10 + int(window * SR), "off", key, 0)], int((window + 0.2) * SR)).mean(1)
    return 20 * np.log10(np.sqrt((y[10:10 + int(window * SR)] ** 2).mean()) + 1e-12)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src")
    ap.add_argument("out")
    ap.add_argument("--release", type=float)
    ap.add_argument("--vels", default="50,120")
    ap.add_argument("--window", type=float, default=0.6)
    a = ap.parse_args()
    vels = [int(v) for v in a.vels.split(",")]

    text = open(a.src, encoding="utf-8").read()
    head, *regions = text.split("<region>")
    layer_of = lambda r: 1 if "xfin_lovel" in r else 0          # crossfaded-in regions are the upper layer
    keys = sorted({int(re.search(r"pitch_keycenter=(\d+)", r)[1]) for r in regions})

    syn = SfizzSynth(os.path.abspath(a.src), SR)
    meas = {(k, li): level(syn, k, v, a.window) for k in keys for li, v in enumerate(vels)}
    target = {li: float(np.median([meas[(k, li)] for k in keys])) for li in range(len(vels))}

    out = []
    for r in regions:
        k = int(re.search(r"pitch_keycenter=(\d+)", r)[1])
        li = min(layer_of(r), len(vels) - 1)
        d = float(np.clip(target[li] - meas[(k, li)], -12, 12))
        r = re.sub(r"volume=(-?[\d.]+)", lambda m: f"volume={float(m[1]) + d:.2f}", r) if "volume=" in r \
            else r.replace("\n", f"\nvolume={d:.2f}\n", 1)
        out.append(r)
        print(f"key {k:3d} layer {li}  {meas[(k, li)]:6.1f} dB  -> {d:+5.1f}")

    if a.release is not None:
        head = re.sub(r"ampeg_release=[\d.]+", f"ampeg_release={a.release}", head)
    src_dir = os.path.relpath(os.path.dirname(os.path.abspath(a.src)), os.path.dirname(os.path.abspath(a.out)))
    src_dir = src_dir.replace("\\", "/") + "/"
    note = f"// evened by tools/sfz_even.py from {os.path.relpath(a.src, ROOT).replace(os.sep, '/')} (per-key level matched to the median)\n"
    with open(a.out, "w", encoding="utf-8") as f:
        f.write(note + f"<control>\ndefault_path={src_dir}\n\n" + head + "<region>".join([""] + out))


if __name__ == "__main__":
    main()
