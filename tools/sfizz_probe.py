"""Probe sfizz_render behaviour: output format, CC11 / CC7 / CC1 response.

Usage: python -I tools/sfizz_probe.py <sfz> <note> <outdir>
"""
import os
import subprocess
import sys

import mido
import numpy as np
import soundfile as sf

SFIZZ = os.path.join(os.path.dirname(__file__), "sfizz", "bin", "Release", "sfizz_render.exe")
sfz, note, outdir = sys.argv[1], int(sys.argv[2]), sys.argv[3]
os.makedirs(outdir, exist_ok=True)
TPB = 480  # 120 BPM default tempo -> 1 beat = 0.5 s


def make_midi(path, ccs, vel=90, beats=8):
    mid = mido.MidiFile(ticks_per_beat=TPB)
    tr = mido.MidiTrack()
    mid.tracks.append(tr)
    events = [(0, mido.Message("note_on", note=note, velocity=vel))]
    for t, cc, val in ccs:
        events.append((t, mido.Message("control_change", control=cc, value=val)))
    events.append((beats * TPB, mido.Message("note_off", note=note, velocity=0)))
    events.sort(key=lambda e: e[0])
    now = 0
    for t, msg in events:
        tr.append(msg.copy(time=t - now))
        now = t
    tr.append(mido.MetaMessage("end_of_track", time=4 * TPB))
    mid.save(path)


def render(name, ccs, vel=90):
    m = os.path.join(outdir, name + ".mid")
    w = os.path.join(outdir, name + ".wav")
    make_midi(m, ccs, vel)
    r = subprocess.run([SFIZZ, "--sfz", sfz, "--midi", m, "--wav", w, "-s", "44100", "--use-eot"],
                       capture_output=True, text=True)
    if r.returncode:
        print(r.stdout, r.stderr)
    x, sr = sf.read(w, always_2d=True)
    info = sf.info(w)
    win = sr // 2
    env = [20 * np.log10(np.sqrt(np.mean(x[i:i + win] ** 2)) + 1e-12) for i in range(0, len(x) - win, win)]
    # spectral centroid of first 2 s
    seg = x[: 2 * sr, 0]
    spec = np.abs(np.fft.rfft(seg * np.hanning(len(seg))))
    freqs = np.fft.rfftfreq(len(seg), 1 / sr)
    centroid = float((spec * freqs).sum() / (spec.sum() + 1e-12))
    print(f"{name:14s} {info.subtype:8s} {sr}Hz peak={20*np.log10(np.abs(x).max()+1e-12):6.1f}dBFS "
          f"centroid={centroid:6.0f}Hz env(0.5s)=" + " ".join(f"{e:5.0f}" for e in env))


render("plain", [])
render("cc11_ramp", [(i * TPB // 2, 11, max(0, 127 - i * 16)) for i in range(9)])
render("cc7_low", [(0, 7, 40)])
render("cc1_0", [(0, 1, 0)])
render("cc1_127", [(0, 1, 127)])
render("vel30", [], vel=30)
