"""Measure every sample in an SFZ map against its pitch_keycenter and write a copy with tune= corrections.

    python -I tools/sfz_retune.py MAP.sfz SAMPLE_BASE_DIR OUT.sfz [--lo KEY] [--hi KEY]

SAMPLE_BASE_DIR is the folder the map's sample= paths are relative to (the program's folder / default_path).
Sample paths are copied unchanged, so OUT must be used from a program whose default_path is that same folder.
Regions outside --lo..--hi are dropped. Pitch comes from the strongest of harmonics 1-3 near the expected
frequency (parabolic peak on a zero-padded FFT of the settled part of the note).
"""
import os
import re
import sys

import numpy as np
import soundfile as sf

MAX_CENTS = 60.0


def measure(path, key):
    x, sr = sf.read(path, always_2d=True)
    m = x.mean(1)
    a = int(0.06 * sr)
    seg = m[a:a + int(0.7 * sr)]
    if len(seg) < int(0.08 * sr):
        seg = m[: int(0.7 * sr)]
    if len(seg) < 256:
        return None
    n = 1 << 18
    spec = np.abs(np.fft.rfft(seg * np.hanning(len(seg)), n))
    df = sr / n
    f0 = 440.0 * 2 ** ((key - 69) / 12)
    best = None
    for h in (1, 2, 3):
        t = f0 * h
        lo, hi = int(t * 2 ** (-80 / 1200) / df), int(t * 2 ** (80 / 1200) / df) + 1
        if hi >= len(spec) - 1:
            break
        i = lo + int(np.argmax(spec[lo:hi]))
        a_, b_, c_ = np.log(spec[i - 1:i + 2] + 1e-12)
        d = 0.5 * (a_ - c_) / (a_ - 2 * b_ + c_) if (a_ - 2 * b_ + c_) != 0 else 0.0
        cents = 1200 * np.log2((i + d) * df / t)
        if best is None or spec[i] > best[0]:
            best = (spec[i], cents)
    return None if best is None else float(best[1])


def main():
    a = sys.argv[1:]
    lo = int(a[a.index("--lo") + 1]) if "--lo" in a else 0
    hi = int(a[a.index("--hi") + 1]) if "--hi" in a else 127
    src, base, out = a[0], a[1], a[2]
    text = open(src, encoding="utf-8", errors="replace").read()
    chunks = re.split(r"(?=<region>)", text)
    head, regions = chunks[0], chunks[1:]
    cache, kept, per_key = {}, [], {}
    for r in regions:
        # values run to the end of the line (sample paths contain '/'); trailing // comments are dropped
        ops = {k: v.split(" //")[0].strip() for k, v in re.findall(r"^\s*([a-z_0-9]+)=(.*)$", r, re.M)}
        key = int(ops.get("pitch_keycenter", ops.get("lokey", ops.get("key", -1))))
        if not lo <= int(ops.get("lokey", ops.get("key", key))) <= hi:
            continue
        sample = ops["sample"].replace("\\", "/")
        if sample not in cache:
            cache[sample] = measure(os.path.join(base, sample), key)
        c = cache[sample]
        if c is None or abs(c) > MAX_CENTS:
            kept.append(r)
            print(f"  key {key}: {sample} not measurable ({c}), left as is")
            continue
        per_key.setdefault(key, []).append(c)
        tune = -round(c) + int(ops.get("tune", 0))
        r = re.sub(r"^\s*tune=\S+\s*$\n?", "", r, flags=re.M)
        kept.append(r.replace("<region>", f"<region>\ntune={tune}", 1))
    note = f"// Retuned copy of {os.path.basename(src)} (tools/sfz_retune.py): per-sample tune= measured against pitch_keycenter.\n"
    open(out, "w", encoding="utf-8").write(note + head + "".join(kept))
    for k in sorted(per_key):
        v = per_key[k]
        print(f"  key {k}: {len(v)} samples, median {np.median(v):+5.1f} c, range {min(v):+5.1f}..{max(v):+5.1f}")
    print(f"{out}: {len(kept)} regions")


if __name__ == "__main__":
    main()
