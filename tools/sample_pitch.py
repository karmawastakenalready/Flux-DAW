"""Duration, peak and estimated fundamental (MIDI note + cents) of one-shot samples.

Usage: python -I tools/sample_pitch.py file1.flac [file2.wav ...]
Pitch: harmonic-product-spectrum over the first 0.25-1.0 s (good enough to set pitch_keycenter/tune).
"""
import os
import sys

import numpy as np
import soundfile as sf


def f0(x, sr, fmin=25, fmax=2000):
    seg = x[int(0.03 * sr): int(0.03 * sr) + int(0.6 * sr)]
    if len(seg) < 2048:
        seg = x[: max(2048, len(x))]
    n = 1 << int(np.ceil(np.log2(len(seg) * 4)))
    spec = np.abs(np.fft.rfft(seg * np.hanning(len(seg)), n))
    freqs = np.fft.rfftfreq(n, 1 / sr)
    hps = spec.copy()
    for h in (2, 3, 4):
        dec = spec[::h]
        hps[: len(dec)] *= dec
    sel = (freqs >= fmin) & (freqs <= fmax)
    k = np.argmax(np.where(sel, hps, 0))
    return freqs[k]


for p in sys.argv[1:]:
    x, sr = sf.read(p, always_2d=True)
    m = x.mean(1)
    pk = 20 * np.log10(np.abs(m).max() + 1e-12)
    f = f0(m, sr)
    midi = 69 + 12 * np.log2(f / 440) if f > 0 else 0
    print(f"{os.path.basename(p):28s} {len(m)/sr:6.2f}s peak {pk:6.1f} dBFS  f0 {f:7.1f} Hz  midi {midi:6.2f}")
