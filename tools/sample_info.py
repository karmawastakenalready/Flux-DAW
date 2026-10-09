"""Summarize sample durations / formats and WAV loop (smpl) chunks in folders.

Usage: python -I tools/sample_info.py folder1 [folder2 ...]
"""
import os
import struct
import sys

import soundfile as sf


def has_smpl_loop(path):
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return False
    i = data.find(b"smpl")
    if i < 0:
        return False
    n_loops = struct.unpack("<I", data[i + 8 + 28:i + 8 + 32])[0]
    return n_loops > 0


for folder in sys.argv[1:]:
    durs, loops, info = [], 0, None
    for fn in sorted(os.listdir(folder)):
        if not fn.lower().endswith((".wav", ".flac")):
            continue
        p = os.path.join(folder, fn)
        i = sf.info(p)
        info = info or f"{i.samplerate}Hz {i.channels}ch {i.subtype}"
        durs.append(i.duration)
        loops += has_smpl_loop(p) if fn.lower().endswith(".wav") else 0
    if durs:
        print(f"{folder:60s} n={len(durs):3d} {info:22s} dur min={min(durs):5.2f}s "
              f"median={sorted(durs)[len(durs)//2]:5.2f}s max={max(durs):5.2f}s loops={loops}")
