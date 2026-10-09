"""Measure the time effects in a rendered Temporal Shift master: time stop envelope + pitch fall, and the
accelerando (audio tempo per 4-bar block).  Usage: python tools/time_fx_check.py output/<title>/<title>.json"""
import json
import os
import sys

import numpy as np
import soundfile as sf

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
from verify import audio_bpm  # noqa: E402

meta = json.load(open(sys.argv[1]))
x, sr = sf.read(os.path.join(os.path.dirname(sys.argv[1]), meta["title"] + "_loop.wav"), dtype="float32")
m = x.mean(1)
tmap = meta.get("tempo_map") or [dict(bar=1, bpm=meta["bpm"], start_s=0.0)]


def bar_time(bar):
    seg = max((t for t in tmap if t["bar"] <= bar), key=lambda t: t["bar"])
    return seg["start_s"] + (bar - seg["bar"]) * 4 * 60.0 / seg["bpm"]


def rms_db(a, b):
    s = m[int(a * sr):int(b * sr)]
    return 20 * np.log10(np.sqrt((s ** 2).mean()) + 1e-12)


def centroid(a, b):
    s = m[int(a * sr):int(b * sr)]
    sp = np.abs(np.fft.rfft(s * np.hanning(len(s))))
    f = np.fft.rfftfreq(len(s), 1 / sr)
    return float((sp * f).sum() / (sp.sum() + 1e-12))


for ts in meta.get("timestops", []):
    a, b, r = ts["stop_s"], ts["freeze_s"], ts["resume_s"]
    pre = a - (b - a)
    print(f"TIME STOP ({ts['bars']})")
    print(f"  before the stop   {rms_db(pre, a):6.1f} dB   centroid {centroid(pre, a):6.0f} Hz")
    q = (b - a) / 4
    for i in range(4):
        print(f"  tape stop {i + 1}/4    {rms_db(a + i * q, a + (i + 1) * q):6.1f} dB   centroid "
              f"{centroid(a + i * q, a + (i + 1) * q):6.0f} Hz")
    print(f"  frozen (3 bars)   {rms_db(b + 0.3, r - 0.3):6.1f} dB   centroid {centroid(b + 0.3, r - 0.3):6.0f} Hz")
    print(f"  resume hit (1 s)  {rms_db(r, r + 1.0):6.1f} dB")

blocks = [(t["bar"], t["bpm"]) for t in tmap[1:]]
if blocks:
    print("ACCELERANDO (audio onset-autocorrelation tempo per block vs written)")
    first = blocks[0][0]
    last = meta["bars"]
    step = 4
    for b0 in range(first, last + 1, step):
        b1 = min(last, b0 + step - 1)
        seg = x[int(bar_time(b0) * sr):int(bar_time(b1 + 1) * sr)]
        written = [bpm for bar, bpm in blocks if b0 <= bar <= b1]
        est = audio_bpm(seg, sr, lo=80, hi=260)
        wavg = np.mean(written)
        est_m = min((est * k for k in (0.5, 1, 2)), key=lambda v: abs(v - wavg))
        print(f"  bars {b0:2d}-{b1:2d}: written {min(written)}-{max(written)} (avg {wavg:.0f})   measured {est_m:6.1f}")
