"""Per-section level (RMS dBFS) of each processed stem (after the piece's MIX gain/EQ) - mix balance check.

Usage: python tools/stem_balance.py output/<title>/<title>.json
"""
import json
import os
import sys

import numpy as np
import soundfile as sf
from pedalboard import Gain, HighpassFilter, Pedalboard

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
import master as M  # noqa: E402
from compose import load_piece  # noqa: E402

meta_path = sys.argv[1]
meta = json.load(open(meta_path))
MIX = load_piece(meta.get("piece", "capital_city")).mix_for(meta.get("version"))
sr = 44100
L = int(round(meta["loop_length_s"] * sr))
tmap = meta.get("tempo_map") or [dict(bar=1, bpm=meta["bpm"], start_s=0.0)]


def bar_time(bar):
    seg = max((t for t in tmap if t["bar"] <= bar), key=lambda t: t["bar"])
    return seg["start_s"] + (bar - seg["bar"]) * 4 * 60.0 / seg["bpm"]


secs = [(int(s["bars"].split("-")[0]), int(s["bars"].split("-")[1]), s["name"].split(" - ")[0]) for s in meta["sections"]]
rows = {}
for name, cfg in MIX.items():
    f = os.path.join(os.path.dirname(meta_path), "stems", name + ".wav")
    if not os.path.exists(f):
        continue
    x, _ = sf.read(f, dtype="float32", always_2d=True)
    if np.abs(x).max() < 1e-7:
        continue
    y = x[:L].copy()
    t = x[L:]
    y[:len(t)] += t[:L]
    for ts in meta.get("timestops", []):
        if name not in ts["frozen_parts"]:
            y = M.tape_stop(y, int(ts["stop_s"] * sr), int(ts["freeze_s"] * sr), int(ts["resume_s"] * sr))
    if cfg.get("duck") and meta.get("duck_triggers"):
        y = y * M.duck_envelope(len(y), meta["duck_triggers"], cfg["duck"][0], cfg["duck"][1], L / sr)[:, None]
    y = M.process(Pedalboard([HighpassFilter(cutoff_frequency_hz=cfg["hpf"]), *M._eq_plugins(cfg["eq"]),
                              Gain(gain_db=cfg["gain"])]), y)
    if cfg.get("fx"):     # same creative chain as master.py (distortion, chorus, delay ...) + post gain
        y = M.process(Pedalboard([*M._fx_plugins(cfg["fx"], L / sr), Gain(gain_db=cfg.get("post_gain", 0.0))]), y)
    rows[name] = [20 * np.log10(np.sqrt((y[int(bar_time(a) * sr):int(bar_time(b + 1) * sr)].mean(1) ** 2).mean()) + 1e-9)
                  for a, b, _ in secs]
print(f"{'stem':12s}" + "".join(f"{s[2][:9]:>10s}" for s in secs))
for n, r in sorted(rows.items(), key=lambda kv: -max(kv[1])):
    print(f"{n:12s}" + "".join(f"{v:10.1f}" for v in r))
