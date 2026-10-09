"""Render candidate CC 'patches' of an SFZ instrument and measure them (level, brightness, attack, release).

Usage: python tools/patch_probe.py   (edit PATCHES below)
"""
import os
import sys

import numpy as np
import soundfile as sf

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
from sfizz_engine import SfizzSynth  # noqa: E402

SR = 44100
I = os.path.join(ROOT, "instruments")
from instruments import CAVEMAN, CAVEMAN_PATCH, _patched_program  # noqa: E402
CAVE = _patched_program("caveman", dict(src=CAVEMAN, patch=CAVEMAN_PATCH), os.path.join(ROOT, "build", "sfz", "probe"))
PATCHES = {
    "cave_default": (CAVE, {}, [40, 52], 2.0),
    "cave_bass": (CAVE, {15: 127, 16: 35, 17: 60, 18: 35, 19: 0, 21: 0, 120: 55, 121: 50, 125: 70, 127: 25,
                         102: 50, 103: 90, 104: 12}, [28, 40], 1.5),
    "cave_lead": (CAVE, {15: 0, 17: 127, 18: 45, 19: 90, 20: 55, 21: 70, 22: 45, 27: 80, 120: 100, 121: 30,
                         111: 25, 104: 30}, [64, 71], 2.0),
    "cave_pad": (CAVE, {15: 40, 16: 15, 17: 110, 18: 65, 19: 90, 20: 15, 21: 70, 22: 25, 100: 70, 104: 70,
                        120: 60, 25: 100, 27: 70}, [52, 59, 64, 67], 3.0),
    "cow_lonely_pad": (os.path.join(I, "karoryfer.cowsynth", "cowsynth_lonely_pad.sfz"), {}, [52, 59, 64], 3.0),
    "cow_shifty_pad": (os.path.join(I, "karoryfer.cowsynth", "cowsynth_shifty_pad.sfz"), {}, [52, 59, 64], 3.0),
    "vocal_poly": (os.path.join(I, "legato_vocal_tutorial", "Programs", "01-no_legato_polyphonic_only_no_vibrato_a.sfz"),
                   {1: 70}, [64], 3.0),
    "vocal_mono": (os.path.join(I, "legato_vocal_tutorial", "Programs", "02-basic_monophonic_no_vibrato_a.sfz"),
                   {1: 70}, [64], 3.0),
    "guitar_ord": (os.path.join(I, "karoryfer.black-and-green-guitars", "Programs", "02-black_keyswitch.sfz"),
                   {}, [40, 47, 52], 2.0),
    "guitar_stac": (os.path.join(I, "karoryfer.black-and-green-guitars", "Programs", "02-black_keyswitch.sfz"),
                    {"ks": 37}, [40, 47], 0.3),
}

out_dir = os.path.join(ROOT, "build", "probe", "patches")
os.makedirs(out_dir, exist_ok=True)
for name, (sfz, ccs, notes, dur) in PATCHES.items():
    s = SfizzSynth(sfz, SR)
    ev = []
    for c, v in ccs.items():
        if c == "ks":
            ev += [(0, "on", v, 100), (50, "off", v, 0)]
        else:
            ev.append((0, "cc", c, v))
    t0 = 200
    for n in notes:
        ev.append((t0, "on", n, 100))
        ev.append((t0 + int(dur * SR), "off", n, 0))
    x = s.render(ev, int((dur + 2.5) * SR))
    s.close()
    sf.write(os.path.join(out_dir, name + ".wav"), x, SR, subtype="FLOAT")
    m = x.mean(1)
    env = np.abs(m)
    pk = env.max()
    a10 = np.argmax(env > pk * 0.1) / SR
    a90 = np.argmax(env > pk * 0.9) / SR
    seg = m[int(0.2 * SR): int(min(dur, 1.2) * SR)] if dur > 0.4 else m[: int(0.3 * SR)]
    sp = np.abs(np.fft.rfft(seg * np.hanning(len(seg))))
    f = np.fft.rfftfreq(len(seg), 1 / SR)
    cen = (sp * f).sum() / (sp.sum() + 1e-12)
    tail = 20 * np.log10(np.sqrt((m[int((dur + 0.5) * SR): int((dur + 1.0) * SR)] ** 2).mean()) + 1e-12)
    rms = 20 * np.log10(np.sqrt((seg ** 2).mean()) + 1e-12)
    print(f"{name:16s} regions={s.num_regions:5d} peak={20*np.log10(pk+1e-12):6.1f} rms={rms:6.1f} "
          f"attack10-90={a10:.3f}-{a90:.3f}s centroid={cen:6.0f}Hz tail(+0.5s)={tail:6.1f}dB")
