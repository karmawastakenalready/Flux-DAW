"""Instrument report cards: render an SFZ through sfizz, measure it, and file the numbers under catalog/.

The composer can't listen, so every instrument is measured before it is used: playable keys, key-to-sounding
offset and tuning, per-key level evenness, brightness and attack, sustain and release, velocity and CC response,
and the license of the library it comes from.

    python -I tools/instrument_card.py SFZ [SFZ ...] [--out catalog/] [--name NAME] [--keys LO-HI] [--ks KEY]
    python -I tools/instrument_card.py --used     # every SFZ the songs use + the VCSL candidates
    python -I tools/instrument_card.py --index    # only rebuild catalog/README.md from the JSON cards

Writes <out>/<slug>.json (all numbers) and regenerates the <out>/README.md index; the flags are explained there.
--keys limits the key scan (e.g. 36-96 or C2-C7). --ks presses a keyswitch before every note (cards that
articulation; without it the file's default articulation is measured). Each SFZ is measured in its own process,
so a file that fails or crashes sfizz only produces an error card.

Every render starts from CC1=70 CC7=100 CC11=127 (as build/far_roads/timbre.py, which measured the reference values).
Metrics (the DAW computes the same ones on in-mix tracks), on the mono note from its onset:
  hi3k_db      10*log10(power above 3 kHz / total power), rfft of the Hann-windowed first 1.0 s
  centroid_hz  power-weighted mean frequency of that spectrum
  attack_db    20*log10(max |x| over 0-30 ms / RMS over 30-300 ms)
"""
import argparse
import ctypes
import datetime
import glob
import hashlib
import html
import json
import os
import re
import subprocess
import sys
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
from sfizz_engine import SfizzSynth, lib  # noqa: E402

SR = 44100
INST = os.path.join(ROOT, "instruments")
BASE_CC = {1: 70, 7: 100, 11: 127}
LEAD = 1024                       # samples between the CCs and the note-on, so CC smoothing has settled
FRAME = 0.02                      # envelope frame (s)
NOTES = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
VCSL_CANDIDATES = ["Folk Harp", "Strumstick", "Psaltery", "Recorder", "Frame Drum"]
HARSH, SHARP, UNEVEN, DETUNED, SHORT, LATE = -35.0, 12.0, 6.0, 15.0, 3.0, 25.0   # flag thresholds (see README_HEAD)


def nn(k):
    return f"{NOTES[k % 12]}{k // 12 - 1}"


def key_num(s):
    """'60', 'C4', 'c#4', 'Eb2' -> MIDI number (C4 = 60)."""
    m = re.fullmatch(r"([A-Ga-g])([#b]?)(-?\d+)", s.strip())
    if not m:
        return int(s)
    return NOTES.index(m[1].upper()) + {"#": 1, "b": -1, "": 0}[m[2]] + 12 * (int(m[3]) + 1)


def db(x):
    return float(20 * np.log10(max(x, 1e-6)))       # -120 dB floor = silence


def rms(x):
    return float(np.sqrt(np.mean(x ** 2))) if len(x) else 0.0


def r(x, n=1):
    return None if x is None else round(float(x), n)


# ---------------------------------------------------------------- measurements on one rendered note

def timbre(m):
    """hi3k_db, centroid_hz, attack_db of a mono note that starts at m[0] (definitions in the docstring)."""
    seg = m[:SR]
    spec = np.abs(np.fft.rfft(seg * np.hanning(len(seg)))) ** 2
    f = np.fft.rfftfreq(len(seg), 1 / SR)
    tot = spec.sum() + 1e-30
    a = np.abs(m[: int(0.03 * SR)]).max()
    b = rms(m[int(0.03 * SR): int(0.3 * SR)])
    pk = np.abs(seg).max()
    return dict(hi3k_db=r(10 * np.log10(spec[f > 3000].sum() / tot + 1e-12)), centroid_hz=r((spec * f).sum() / tot, 0),
                attack_db=r(20 * np.log10(max(a, 1e-12) / (b + 1e-9))),
                onset_ms=r(np.argmax(np.abs(seg) >= 0.1 * pk) / SR * 1000) if pk > 0 else None)  # first -20 dB re. peak


def yin(m, fmin, fmax, w=2048, hop=1024, thr=0.15):
    """YIN f0 of build/wanderer/probe.py (difference function via FFT): median over the frames in 0.04-0.5 s that
    are within 30 dB of the loudest (short notes have died by 0.1 s).
    Returns (hz, aperiodicity); aperiodicity near 0 = clearly pitched, near 1 = noise."""
    lo, hi = max(2, int(SR / fmax)), int(SR / fmin) + 2
    n = 1 << int(np.ceil(np.log2(w + hi)))
    tau = np.arange(hi)
    frames = [m[st:st + w + hi] for st in range(int(0.04 * SR), int(0.5 * SR), hop) if st + w + hi <= len(m)]
    loud = max([rms(s) for s in frames] + [0.0])
    fs, aps = [], []
    for seg in frames:
        if rms(seg) < max(loud * 10 ** (-30 / 20), 1e-6):
            continue
        corr = np.fft.irfft(np.fft.rfft(seg, n) * np.conj(np.fft.rfft(seg[:w], n)), n)[:hi]
        c = np.concatenate(([0.0], np.cumsum(seg ** 2)))
        d = c[w] + c[tau + w] - c[tau] - 2 * corr
        cm = np.ones(hi)
        cm[1:] = d[1:] * tau[1:] / (np.cumsum(d[1:]) + 1e-12)
        below = np.nonzero(cm[lo:] < thr)[0]
        i = lo + (below[0] if len(below) else int(np.argmin(cm[lo:])))
        while i + 1 < hi and cm[i + 1] < cm[i]:
            i += 1
        # the first dip can be a strong upper partial (period / 2 or / 3): take a clearly deeper dip at 2x or 3x the lag
        for mult in (2, 3):
            j0, j1 = int(i * mult * 0.97), min(hi, int(i * mult * 1.03) + 2)
            if j0 < j1:
                j = j0 + int(np.argmin(cm[j0:j1]))
                if cm[j] < 0.5 * cm[i] and cm[i] - cm[j] > 0.05:   # not just noise between two near-zero dips
                    i = j
        dd = 0.0
        if 0 < i < hi - 1:
            a, b, cc = cm[i - 1], cm[i], cm[i + 1]
            dd = 0.5 * (a - cc) / (a - 2 * b + cc) if (a - 2 * b + cc) else 0.0
        fs.append(SR / (i + dd))
        aps.append(cm[i])
    return (float(np.median(fs)), float(np.median(aps))) if fs else (0.0, 1.0)


def refine(m, f):
    """Above 400 Hz a period is only a few samples long: take the zero-padded spectrum peak within 50 cents."""
    seg = m[int(0.04 * SR): int(0.5 * SR)]
    n = 1 << 18
    spec = np.abs(np.fft.rfft(seg * np.hanning(len(seg)), n))
    k0, k1 = int(f * 2 ** (-1 / 24) * n / SR), int(f * 2 ** (1 / 24) * n / SR) + 1
    k = k0 + int(np.argmax(spec[k0:k1]))
    if k in (k0, k1 - 1):
        return f
    a, b, c = np.log(spec[k - 1:k + 2] + 1e-20)
    return (k + (0.5 * (a - c) / (a - 2 * b + c) if (a - 2 * b + c) else 0.0)) * SR / n


def subharmonic(m, f):
    """With a weak fundamental YIN can lock onto the 2nd harmonic (some folk harp notes). If the spectrum has clear
    partials at two of f/2, 1.5 f and 2.5 f, the note is f/2. (Checking for f/3 the same way misfired more than it
    helped.)"""
    seg = m[int(0.04 * SR): int(0.5 * SR)]
    n = 1 << 18
    spec = np.abs(np.fft.rfft(seg * np.hanning(len(seg)), n)) ** 2

    def peak(fr, cents):
        k0, k1 = int(fr * 2 ** (-cents / 1200) * n / SR), int(fr * 2 ** (cents / 1200) * n / SR) + 1
        return spec[k0:k1] if fr > 20 and k1 < len(spec) else np.zeros(1)

    ref = peak(f, 30).max()
    # a partial counts if it is within 30 dB of the one at f and stands 10 dB above its own surroundings
    hits = sum(peak(f * j, 30).max() > max(ref * 1e-3, 10 * np.median(peak(f * j, 100))) for j in (0.5, 1.5, 2.5))
    return f / 2 if hits >= 2 else f


def envelope(y):
    n = int(FRAME * SR)
    k = len(y) // n
    return 20 * np.log10(np.maximum(np.sqrt((y[:k * n].reshape(k, n) ** 2).mean(1)), 1e-6))


class Player:
    """One sfizz instance; plays a single note per render and cuts all voices afterwards so renders don't bleed."""

    def __init__(self, path, ks=None):
        L = lib()
        L.sfizz_all_sound_off.argtypes = [ctypes.c_void_p]
        self.syn = SfizzSynth(path, SR)
        self.ks = ks

    def play(self, key, vel=90, hold=1.0, tail=0.05, ccs=None, stop=False):
        ev = [(0, "cc", c, v) for c, v in {**BASE_CC, **(ccs or {})}.items()]
        if self.ks is not None:
            ev += [(LEAD - 400, "on", self.ks, 100), (LEAD - 200, "off", self.ks, 0)]
        off = LEAD + int(hold * SR)
        ev += [(LEAD, "on", key, vel), (off, "off", key, 0)]
        y = self.syn.render(ev, off + int(tail * SR), stop_on_silence_after=off + 64 if stop else None)
        lib().sfizz_all_sound_off(self.syn.h)
        return y.mean(1).astype(np.float64)[LEAD:]

    def labels(self):
        """Key and CC names the SFZ declares (sw_label, label_key, label_ccN), via sfizz's MIDNAM export."""
        L = lib()
        L.sfizz_export_midnam.restype = ctypes.c_void_p
        L.sfizz_export_midnam.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        L.sfizz_free_memory.argtypes = [ctypes.c_void_p]
        ptr = L.sfizz_export_midnam(self.syn.h, b"card")
        xml = ctypes.string_at(ptr).decode("utf-8", "replace")
        L.sfizz_free_memory(ptr)
        keys = {n: html.unescape(v) for n, v in re.findall(r'<Note Number="(\d+)" Name="([^"]*)"', xml)}
        ccs = {n: html.unescape(v) for n, v in re.findall(r'<Control Type="\w+" Number="(\d+)" Name="([^"]*)"', xml)
               if n not in ("7", "10", "11")}
        return keys, ccs

    def close(self):
        self.syn.close()


# ---------------------------------------------------------------- card sections

def runs(keys, bridge=0):
    out = []
    for k in keys:
        if out and k - out[-1][1] <= bridge + 1:
            out[-1][1] = k
        else:
            out.append([k, k])
    return out


def keymap(sounding):
    ranges = runs(sounding)
    main = max(runs(sounding, bridge=2), key=lambda a: a[1] - a[0])   # longest range, bridging 1-2 key holes
    inside = lambda k: main[0] <= k <= main[1]
    return dict(main=main, main_names=f"{nn(main[0])}-{nn(main[1])}", ranges=ranges, count=len(sounding),
                span=[sounding[0], sounding[-1]],
                gaps=[k for k in range(main[0], main[1] + 1) if k not in sounding],
                other_ranges=[a for a in ranges if a[1] - a[0] >= 2 and not inside(a[0])],
                isolated=[k for a in ranges if a[1] - a[0] < 2 and not inside(a[0]) for k in range(a[0], a[1] + 1)])


def key_detail(p, k):
    y = p.play(k, 90, hold=0.7)
    fk = 440 * 2 ** ((k - 69) / 12)
    f, ap = yin(y, max(25.0, fk / 8), min(5000.0, fk * 8))
    f = subharmonic(y, f) if f > 0 else f
    if f > 400:
        f = refine(y, f)
    d = dict(level_db=r(db(rms(y[: int(0.6 * SR)]))), ap=r(ap, 2))
    if f > 0:
        midi = 69 + 12 * np.log2(f / 440)
        d.update(f0=r(f, 2), note=int(round(midi)), cents=r(100 * (midi - round(midi))))
    return d


def pitch_section(per):
    """kind: pitched (one key-to-sounding offset), mixed (pitched, but the offset changes across the range, e.g. an
    organ with several footages) or unpitched (drum / FX map)."""
    sure = {k: d for k, d in per.items() if "note" in d and d["ap"] < 0.25 and d["level_db"] > -90}
    out = dict(kind="unpitched", confident_keys=len(sure))
    if len(sure) < max(3, 0.5 * len(per)):
        return out
    offs = Counter(d["note"] - k for k, d in sure.items()).most_common(4)
    off, n = offs[0]
    if n >= 0.5 * len(sure):
        out.update(kind="pitched", offset=off, offset_share=r(n / len(sure), 2),
                   offset_outliers={str(k): d["note"] - k for k, d in sure.items() if d["note"] - k != off})
        # a key a semitone off is badly tuned (or scoops up into its note), not mapped to another octave
        sure = {k: dict(d, cents=r(100 * (d["note"] - k - off) + d["cents"]))
                for k, d in sure.items() if abs(d["note"] - k - off) <= 1}
    elif len(offs) > 1 and offs[1][1] >= 2 and n + offs[1][1] >= 0.6 * len(sure):
        out.update(kind="mixed", offsets={str(o): c for o, c in offs})
    else:   # pitched-sounding drums whose pitch doesn't follow the keys
        return dict(out, offsets={str(o): c for o, c in offs})
    worst = max(sure, key=lambda k: abs(sure[k]["cents"]))
    return dict(out, worst_cents=sure[worst]["cents"], worst_key=worst,
                median_abs_cents=r(np.median([abs(d["cents"]) for d in sure.values()])),
                out_of_tune={str(k): d["cents"] for k, d in sure.items() if abs(d["cents"]) > DETUNED})


def level_section(per, main_keys):
    lv = np.array([per[k]["level_db"] for k in main_keys])
    out = dict(median_db=r(np.median(lv)), std_db=r(lv.std()), min_db=r(lv.min()), max_db=r(lv.max()))
    if len(lv) > 1:
        jumps = np.diff(lv)
        i = int(np.argmax(np.abs(jumps)))
        out.update(max_jump_db=r(abs(jumps[i])), max_jump_keys=[main_keys[i], main_keys[i + 1]],
                   jumps_over_6db=int((np.abs(jumps) > UNEVEN).sum()))
    return out


def reps(keys):
    """A few representative keys per octave (nearest sounding keys to C, E, G#)."""
    out = []
    for o in sorted({k // 12 for k in keys}):
        ks = [k for k in keys if k // 12 == o]
        for t in (12 * o, 12 * o + 4, 12 * o + 8):
            k = min(ks, key=lambda k: abs(k - t))
            if k not in out:
                out.append(k)
    return out


def timbre_section(p, keys, core):
    notes = []
    for k in reps(keys):
        y = p.play(k, 70, hold=1.05)
        if np.abs(y).max() > 1e-6:
            notes.append(dict(key=k, **timbre(y)))
    med = lambda ns: {m: r(np.median([n[m] for n in ns if n[m] is not None]), 0 if m == "centroid_hz" else 1)
                      for m in ("hi3k_db", "centroid_hz", "attack_db", "onset_ms")} if ns else None
    octs = {}
    for n in notes:
        octs.setdefault(nn(n["key"] // 12 * 12), []).append(n)
    in_core = [n for n in notes if core[0] <= n["key"] <= core[1]] or notes
    return dict(velocity=70, main_keys=core, main=med(in_core), median=med(notes),
                octaves={o: med(ns) for o, ns in octs.items()}, notes=notes)


def sustain_section(p, key):
    rel = envelope(p.play(key, 90, hold=6.0)[: 6 * SR])
    rel -= rel.max()
    ip = int(np.argmax(rel))
    span = lambda t0, t1: float(np.median(rel[int(t0 / FRAME):int(t1 / FRAME)]))
    down = np.nonzero(rel[ip:] < -40)[0]
    t40, at2s = r((ip + down[0]) * FRAME, 2) if len(down) else None, span(1.5, 2.5)
    # 2 s in, a held note (bowed, blown, looped) is still near its peak; a plucked or struck one has died away
    if at2s > -10:
        # the recording has run out once the (100 ms smoothed) level stays 10 dB under its sustain level for good
        up = np.nonzero(np.convolve(rel, np.ones(5) / 5, mode="same") >= span(0.5, 2.5) - 10)[0]
        end = r((up[-1] + 1) * FRAME, 2) if len(up) and up[-1] < len(rel) - 5 else None
        kind, usable = ("sustained", None) if end is None else ("sustained, sample ends", end)
    else:
        kind, usable = "decaying", t40
    # release: hold 1 s, then the time until the tail is 60 dB under its level at the note-off
    y = p.play(key, 90, hold=1.0, tail=10.0, stop=True)
    off = int(1.0 * SR)
    ref = db(rms(y[off - int(0.05 * SR):off]))
    rel_s = None
    if ref > db(np.abs(y[:off]).max()) - 60:
        above = np.nonzero(envelope(y[off:]) > ref - 60)[0]
        rel_s = r((above[-1] + 1) * FRAME if len(above) else 0.0, 2)
    return dict(key=key, kind=kind, peak_at_s=r(ip * FRAME, 2), t40_s=t40, level_at_2s_db=r(at2s),
                usable_max_note_s=usable, release_s=rel_s,
                note="times from note-on; t40/usable None = still sounding after 6 s; release_s 10.0 = still sounding "
                     "10 s after note-off, None = already silent at note-off")


def velocity_section(p, key):
    lv = {}
    for v in (30, 70, 110):
        y = p.play(key, v, hold=1.05)
        lv[str(v)] = dict(rms_db=r(db(rms(y[: int(0.6 * SR)]))), **timbre(y))
    rng = lv["110"]["rms_db"] - lv["30"]["rms_db"]
    dh = lv["110"]["hi3k_db"] - lv["30"]["hi3k_db"]
    dc = lv["110"]["centroid_hz"] / max(lv["30"]["centroid_hz"], 1)
    kind = ("no velocity response" if abs(rng) < 1.5 else
            "level and timbre (layers or filter)" if abs(dh) >= 3 or not 0.83 < dc < 1.2 else "level only")
    return dict(key=key, levels=lv, range_db=r(rng), hi3k_change_db=r(dh), kind=kind)


def cc_section(p, key):
    out = {}
    for cc in (1, 11):
        y0, y1 = (p.play(key, 90, hold=0.65, ccs={cc: v}) for v in (0, 127))
        l0, l1 = db(rms(y0[: int(0.6 * SR)])), db(rms(y1[: int(0.6 * SR)]))
        if l0 < -90:
            out[str(cc)] = dict(level_change_db=None, hi3k_change_db=None, silent_at_0=True, changes_level=True,
                                changes_timbre=None)
            continue
        dl, dh = l1 - l0, timbre(y1)["hi3k_db"] - timbre(y0)["hi3k_db"]
        out[str(cc)] = dict(level_change_db=r(dl), hi3k_change_db=r(dh), silent_at_0=False,
                            changes_level=abs(dl) > 1, changes_timbre=abs(dh) > 2)
    return out


# ---------------------------------------------------------------- SFZ text: includes, keyswitches, license

def sfz_text(path, root_dir=None, depth=0):
    """The SFZ with its #include files inlined (read as text only, never executed)."""
    root_dir = root_dir or os.path.dirname(path)
    text = open(path, encoding="utf-8", errors="replace").read()
    if depth > 8:
        return text

    def inc(m):
        for base in (root_dir, os.path.dirname(path)):
            p = os.path.normpath(os.path.join(base, m[1]))
            if os.path.isfile(p):
                return sfz_text(p, root_dir, depth + 1)
        return ""
    return re.sub(r'#include\s+"([^"]+)"', inc, text)


def about(text):
    lines = [ln.strip("/ ").strip() for ln in text.splitlines()[:3] if ln.startswith("//")]
    return " ".join(lines)[:240] or None


def keyswitches(text, labels):
    vals = lambda op: sorted({key_num(v) for v in re.findall(rf"\b{op}=([A-Ga-g][#b]?-?\d|\d+)", text)})
    ks = vals("sw_last")
    if not ks:
        return None
    return dict(keys={str(k): labels.get(str(k), "") for k in ks}, default=(vals("sw_default") or [None])[0])


LICENSES = [  # most specific first
    ("CC BY-NC-SA", r"Attribution-?NonCommercial-?ShareAlike|\bBY-NC-SA\b"),
    ("CC BY-NC", r"Attribution-?NonCommercial|\bBY-NC\b"),
    ("CC BY-SA", r"Attribution-?ShareAlike|\bBY-SA\b"),
    ("CC0 1.0", r"\bCC0\b|CC-0|Creative Commons Zero|publicdomain/zero"),
    ("CC BY", r"\bAttribution \d\.\d|\bCC[- ]BY\b"),
    ("GeneralUser GS License", r"GeneralUser GS.{0,40}License"),
    ("MIT", r"\bMIT License\b"),
    ("GPL", r"GNU General Public License|\bGPL\b"),
    ("BSD", r"\bBSD\b"),
    ("public domain", r"\bpublic domain\b"),
]


def license_name(text):
    """License named in a LICENSE/README text: a 'Samples' section first (multi-part licenses), then the title."""
    parts = [m[1] for m in re.finditer(r"^#+\s*Samples?\b[^\n]*\n(.*?)(?=^#+\s|\Z)", text, re.M | re.S | re.I)]
    for part in parts + [text[:800], text]:
        for name, pat in LICENSES:
            m = re.search(pat, part, re.I if name == "public domain" else 0)
            if m:
                v = re.search(r"Attribution (\d\.\d)|CC[- ]BY[- ](\d\.\d)", part) if name == "CC BY" else None
                return f"CC BY {v[1] or v[2]}" if v else name
    return None


def norm(s):
    return re.sub(r"[^a-z0-9]", "", s.lower())


def licenses_md_row(lib_name):
    """The license LICENSES.md states for a library (matched by folder name in the row, e.g. its repo URL)."""
    p = os.path.join(ROOT, "LICENSES.md")
    if not os.path.isfile(p):
        return None
    for line in open(p, encoding="utf-8").read().splitlines():
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if line.startswith("|") and len(cells) >= 3 and norm(lib_name) in norm(line):
            return cells[2].replace("**", "")
    return None


def license_of(path, text):
    """Library folders this SFZ draws from (its own location, default_path=, sample=) and their licenses."""
    sdir = os.path.dirname(path)
    refs = set(re.findall(r"(?:default_path|sample)=(.+?)(?=\s+\w+=|\s*<|\s*$)", text, re.M))
    dps = [v for v in re.findall(r"default_path=(.+?)\s*$", text, re.M)]
    libs = []
    for ref in [path] + sorted(refs)[:3000]:
        ref = ref.replace("\\", "/")
        for c in [ref, os.path.join(sdir, ref)] + [os.path.join(sdir, d.replace("\\", "/"), ref) for d in dps]:
            try:
                rp = os.path.relpath(os.path.normpath(c), INST)
            except ValueError:
                continue
            top = rp.split(os.sep)[0]
            if not rp.startswith("..") and os.sep in rp and top not in libs and os.path.isdir(os.path.join(INST, top)):
                libs.append(top)
    out = []
    for lib_name in libs:
        d = os.path.join(INST, lib_name)
        files = sorted(f for f in os.listdir(d) if re.match(r"(licen[cs]e|copying)", f, re.I)) + \
            sorted(f for f in os.listdir(d) if re.match(r"readme", f, re.I)) if os.path.isdir(d) else []
        name, src = None, None
        for f in files:
            name = license_name(open(os.path.join(d, f), encoding="utf-8", errors="replace").read(200_000))
            if name:
                src = os.path.relpath(os.path.join(d, f), ROOT).replace(os.sep, "/")
                break
        md = licenses_md_row(lib_name)
        out.append(dict(library=lib_name, license=name or "unknown", file=src, licenses_md=md or "not listed",
                        agrees=bool(name and md and norm(name) in norm(md))))
    names = list(dict.fromkeys(o["license"] for o in out)) or ["unknown"]
    return dict(name=" + ".join(names), libraries=out, all_in_licenses_md=bool(out) and all(o["agrees"] for o in out))


# ---------------------------------------------------------------- one card

def flags(card):
    f = {}
    if card.get("error"):
        return {"error": card["error"]}
    t = (card.get("timbre") or {}).get("main") or {}
    s = card.get("sustain") or {}
    if t.get("hi3k_db") is not None and t["hi3k_db"] > HARSH:
        f["harsh"] = f"hi3k {t['hi3k_db']} dB in the main range (> {HARSH})"
    if t.get("attack_db") is not None and t["attack_db"] > SHARP:
        f["sharp attack"] = f"attack {t['attack_db']} dB (> {SHARP})"
    # bowed and blown notes swell in by nature; a plucked or struck one that starts late has silence in its samples
    if s.get("kind") == "decaying" and t.get("onset_ms") is not None and t["onset_ms"] > LATE:
        f["late start"] = f"the attack arrives {t['onset_ms']} ms after note-on (> {LATE:.0f} ms)"
    pitch, lv = card.get("pitch") or {}, card.get("level") or {}
    if pitch.get("kind") in ("pitched", "mixed") and lv.get("max_jump_db", 0) > UNEVEN:   # a kit's sounds differ anyway
        a, b = lv["max_jump_keys"]
        f["uneven"] = (f"{lv['max_jump_db']} dB jump between keys {a} and {b} ({lv['jumps_over_6db']} jumps > {UNEVEN} dB);"
                       f" even it out with tools/sfz_even.py")
    if pitch.get("out_of_tune"):
        f["out of tune"] = f"{len(pitch['out_of_tune'])} keys > {DETUNED} cents, worst {pitch['worst_cents']:+} at key {pitch['worst_key']}"
    if s.get("kind", "").startswith("sustained") and s.get("usable_max_note_s") and s["usable_max_note_s"] < SHORT:
        f["short sustain"] = f"the recording of a held note runs out after {s['usable_max_note_s']} s"
    if card.get("errors"):
        f["partial"] = "some measurements failed: " + ", ".join(card["errors"])
    return f


def measure(path, name, slug, lo=0, hi=127, ks=None):
    text = sfz_text(path)
    card = dict(name=name, slug=slug, sfz=os.path.relpath(path, ROOT).replace(os.sep, "/"), about=about(text),
                measured=str(datetime.date.today()), conditions=dict(sr=SR, cc=BASE_CC, keyswitch=ks, keys=[lo, hi]),
                license=license_of(path, text), errors={})
    try:
        p = Player(path, ks)
    except Exception as e:
        card["error"] = f"load failed: {e}"
        return card
    try:   # coarse scan on its own synth: keyswitches pressed here would change the articulation measured later
        peak = {k: db(np.abs(p.play(k, 90, hold=0.25)).max()) for k in range(lo, hi + 1)}
    finally:
        p.close()
    top = max(peak.values())
    sounding = [k for k, v in peak.items() if v > max(-80.0, top - 50.0)]
    if not sounding:
        card["error"] = "no key makes a sound"
        return card
    card["keys"] = keymap(sounding)
    main = card["keys"]["main"]
    main_keys = [k for k in sounding if main[0] <= k <= main[1]]

    p = Player(path, ks)
    try:
        card["regions"] = p.syn.num_regions
        klabels, card["cc_labels"] = p.labels()
        card["key_labels"] = klabels
        card["keyswitches"] = keyswitches(text, klabels)

        def section(key, fn, *a):
            try:
                card[key] = fn(*a)
            except Exception as e:
                card["errors"][key] = f"{type(e).__name__}: {e}"
        per = {}
        section("per_key", lambda: {str(k): per.setdefault(k, key_detail(p, k))
                                    for k in main_keys + [k for k in sounding if k not in main_keys]})
        section("pitch", pitch_section, per)
        section("level", level_section, per, main_keys)
        if (card.get("pitch") or {}).get("kind", "unpitched") != "unpitched":
            # main range = the middle half of the playable keys; the test note sits in its middle
            q1, q3 = np.percentile(main_keys, [25, 75])
            core = [k for k in main_keys if q1 <= k <= q3] or main_keys
            mid = min(core, key=lambda k: abs(k - np.median(core)))
            section("timbre", timbre_section, p, main_keys, [core[0], core[-1]])
        else:   # drum / FX map: every key is its own sound; test the loudest
            mid = max(sounding, key=lambda k: per.get(k, {}).get("level_db", -120))
            section("timbre", timbre_section, p, sounding, [sounding[0], sounding[-1]])
        section("sustain", sustain_section, p, mid)
        section("velocity", velocity_section, p, mid)
        section("cc", cc_section, p, mid)
    finally:
        p.close()
    return card


# ---------------------------------------------------------------- jobs, catalog, README

def ident(path):
    """Readable name for an SFZ path: far_roads/flute, VCSL/Folk Harp, sfz/makima_kit, makima/synth_bass."""
    parts = os.path.relpath(path, ROOT).replace(os.sep, "/").split("/")
    stem = os.path.splitext(parts[-1])[0]
    if parts[:2] == ["build", "sfz"]:
        sub = parts[2:-1]
        name = "build/" + stem if not sub else f"{sub[0]}/{sub[1] if len(sub) > 1 else stem}"
    elif parts[0] == "instruments" and len(parts) > 2:
        name = f"{parts[1]}/{stem}"
    elif parts[0] == "sfz":
        name = f"sfz/{stem}"
    else:
        name = stem
    return name


def slugify(name):
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")


def content_key(path):
    """Identical wrappers (same text after #includes, absolute default_path) are one instrument."""
    text = sfz_text(path)
    dps = re.findall(r"default_path=(.+?)\s*$", text, re.M)
    absolute = dps and all(re.match(r"([A-Za-z]:)?[/\\]", d) for d in dps)
    return hashlib.sha1(text.encode()).hexdigest() + ("" if absolute else os.path.normcase(os.path.dirname(path)))


def gather_used():
    """[(path, [where used])] for the songs (Flux projects, sfz/, instruments.py sets) and the VCSL candidates."""
    use = {}

    def add(p, where):
        p = os.path.abspath(p)
        use.setdefault(os.path.normcase(p), (p, []))[1].append(where)
    for proj in sorted(glob.glob(os.path.join(ROOT, "daw", "projects", "*.flux.json"))):
        song = os.path.basename(proj).split(".")[0]
        for t in json.load(open(proj, encoding="utf-8")).get("tracks", []):
            ins = t.get("instrument") or {}
            if ins.get("kind") == "sfz" and ins.get("path"):
                add(os.path.join(ROOT, ins["path"]), f"{song}: {t.get('name')}")
    for p in sorted(glob.glob(os.path.join(ROOT, "sfz", "*.sfz"))):
        add(p, "sfz/")
    import instruments
    for s in instruments.INSTRUMENT_SETS:
        try:
            for n, p in instruments.prepare_sfz(s).items():
                add(p, f"{s}/{n}")
        except Exception as e:
            print(f"instruments.py set {s}: {e}", file=sys.stderr)
    for p in sorted(glob.glob(os.path.join(INST, "VCSL", "**", "*.sfz"), recursive=True)):
        if any(c in os.path.basename(p) for c in VCSL_CANDIDATES):
            add(p, "VCSL candidate")
    return list(use.values())


def dedupe(entries):
    groups = {}
    for p, where in entries:
        try:
            key = content_key(p)
        except OSError:
            key = os.path.normcase(p)
        g = groups.setdefault(key, dict(path=p, used_by=[], aliases=[]))
        g["used_by"] += [w for w in where if w not in g["used_by"]]
        if os.path.normcase(p) != os.path.normcase(g["path"]):
            g["aliases"].append(os.path.relpath(p, ROOT).replace(os.sep, "/"))
    return list(groups.values())


def run_job(job, out, timeout=900):
    """Measure one SFZ in a child process (a sfizz crash or hang only costs this card)."""
    cmd = [sys.executable, "-I", os.path.abspath(__file__), job["path"], "--worker", job["slug"], "--out", out,
           "--name", job["name"]] + (["--keys", job["keys"]] if job.get("keys") else []) + \
          (["--ks", str(job["ks"])] if job.get("ks") is not None else [])
    dst = os.path.join(out, job["slug"] + ".json")
    t0, err = time.time(), None
    try:
        res = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout)
        if res.returncode:
            err = (res.stderr.strip().splitlines() or [f"exit code {res.returncode}"])[-1]
    except subprocess.TimeoutExpired:
        err = f"timed out after {timeout} s"
    card = json.load(open(dst, encoding="utf-8")) if os.path.isfile(dst) and os.path.getmtime(dst) >= t0 else None
    if card is None:
        card = dict(name=job["name"], slug=job["slug"], sfz=os.path.relpath(job["path"], ROOT).replace(os.sep, "/"),
                    measured=str(datetime.date.today()), error=err or "no card written")
        try:
            card["license"] = license_of(job["path"], sfz_text(job["path"]))
        except OSError:
            pass
        card["flags"] = flags(card)
    if job.get("used_by") is not None:
        card["used_by"], card["aliases"] = job["used_by"], job["aliases"]
    card["seconds"] = round(time.time() - t0, 1)
    write(card, dst)
    return card


def write(card, dst):
    with open(dst, "w", encoding="utf-8") as f:
        json.dump(card, f, indent=1, ensure_ascii=False)


README_HEAD = """# Instrument catalog

One report card per sampled instrument (SFZ): every instrument the songs use, plus candidates. The composer can't
listen, so instruments are measured here before they go into a song. Each name links to its JSON card with all the
numbers (per-key pitch and level, brightness per octave, velocity and CC response, keyswitches, license).

Regenerate everything with `python -I tools/instrument_card.py --used`, or card one file with
`python -I tools/instrument_card.py path/to/file.sfz`.

## Flags

| Flag | What it means | Threshold |
|---|---|---|
| harsh | Too much energy above 3 kHz, so it sounds thin and piercing. The rejected lyre measured -28.5 dB; the liked VSCO harp -49.6 dB. The verdicts behind this threshold came from plucked instruments: bowed strings, reeds and brass are naturally brighter (about -10 to -30 dB), so compare those within their own family | hi3k_db > {harsh:.0f} dB in the main range |
| sharp attack | The start of each note is a spiky click well above the body of the note. The rejected zither melody strings measured 17.9 dB | attack > {sharp:.0f} dB |
| uneven | Some notes are much louder or softer than their neighbours. Fix: `tools/sfz_even.py` | neighbouring keys differ by more than {uneven:.0f} dB |
| out of tune | Some notes are off pitch | more than {detuned:.0f} cents (a semitone is 100) |
| short sustain | Held notes die away early because the recordings are short | a held note's recording runs out before {short:.0f} s |
| late start | A plucked or struck note whose recording begins with silence, so it sounds behind the beat (it also makes its attack number meaningless) | attack arrives more than {late:.0f} ms after note-on |
| partial / error | Some measurements, or the whole card, failed. See the JSON | |

## Columns

- **keys**: main playable range (C4 = MIDI 60). "+N isolated" = single keys outside it (keyswitches, FX); "N gaps" = silent keys inside it. Kits list every key that sounds.
- **offset**: sounding note minus played key, in semitones. +12 means it sounds an octave above the key you write. "kit" = unpitched (drum or FX map); "mixed" = pitched, but the octave changes across the range (e.g. an organ with several stops). Single keys that sound in another octave are listed in the JSON (`offset_outliers`).
- **tuning**: the worst pitch error of any key, in cents, over the first half second (so a note that starts flat and scoops up into pitch counts as out of tune). Bells, glockenspiel and timpani have inharmonic overtones, so their pitch, offset and tuning readings are rough.
- **evenness**: the biggest level jump between neighbouring keys (sd = spread of all key levels), at velocity 90.
- **hi3k_db / attack**: brightness and attack spike, median over the main range at velocity 70 (lower = softer).
- **sustain**: "held" = the note sustains while the key is down ("6 s+" = still going after 6 s, "held 4.5 s" = the recording runs out after 4.5 s); "rings" = a plucked or struck note dies away (time to -40 dB). "rel" = time to fade after the key is released.
- **license**: from the library's own LICENSE or README. A dagger (†) means the library is missing from `LICENSES.md`, so add it before shipping.

## Cards

| Name | Keys | Offset | Tuning | Evenness | hi3k_db | Attack | Sustain | Flags | License |
|---|---|---|---|---|---|---|---|---|---|
"""


def row(c):
    lic = c.get("license") or {}
    lic_s = lic.get("name", "unknown") + ("" if lic.get("all_in_licenses_md") else " †")
    if c.get("error"):
        return f"| [{c['name']}]({c['slug']}.json) | | | | | | | | error: {c['error'][:80]} | {lic_s} |"
    k, p, lv, t, s = (c.get(x) or {} for x in ("keys", "pitch", "level", "timbre", "sustain"))
    tonal = p.get("kind") in ("pitched", "mixed")
    if tonal and k.get("main"):
        keys = f"{k['main_names']} ({k['main'][0]}-{k['main'][1]})"
        keys += f" +{len(k['isolated'])} isolated" if k.get("isolated") else ""
        keys += f", {len(k['gaps'])} gaps" if k.get("gaps") else ""
    elif k.get("span"):
        keys = f"{nn(k['span'][0])}-{nn(k['span'][1])}: {k['count']} keys"
    else:
        keys = ""
    off = f"{p['offset']:+d}" if p.get("kind") == "pitched" else "mixed" if tonal else "kit"
    tun = f"{p['worst_cents']:+.0f}c" if tonal else "-"
    ev = f"{lv['max_jump_db']} dB (sd {lv['std_db']})" if tonal and "max_jump_db" in lv else "-"
    tm = t.get("main") or {}
    if s.get("kind", "").startswith("sustained"):
        sus = "held " + (f"{s['usable_max_note_s']} s" if s.get("usable_max_note_s") else "6 s+")
    elif s:
        sus = "rings " + (f"{s['t40_s']} s" if s.get("t40_s") else "6 s+")
    else:
        sus = ""
    if s.get("release_s") is not None:
        sus += ", rel 10 s+" if s["release_s"] >= 9.98 else f", rel {s['release_s']} s"
    att = tm.get("attack_db", "")
    att = ">60" if isinstance(att, float) and att > 60 else att   # a click that is over within 30 ms
    fl = ", ".join(c.get("flags") or {}) or "-"
    return (f"| [{c['name']}]({c['slug']}.json) | {keys} | {off} | {tun} | {ev} | {tm.get('hi3k_db', '')} | "
            f"{att} | {sus} | {fl} | {lic_s} |")


def index(out):
    cards = []
    for f in sorted(glob.glob(os.path.join(out, "*.json"))):
        try:
            cards.append(json.load(open(f, encoding="utf-8")))
        except (OSError, ValueError):
            continue
    cards.sort(key=lambda c: c.get("name", "").lower())
    head = README_HEAD.format(harsh=HARSH, sharp=SHARP, uneven=UNEVEN, detuned=DETUNED, short=SHORT, late=LATE)
    with open(os.path.join(out, "README.md"), "w", encoding="utf-8") as fh:
        fh.write(head + "\n".join(row(c) for c in cards) + "\n")
    return cards


def summary(c):
    fl = "; ".join(f"{k}: {v}" for k, v in (c.get("flags") or {}).items()) or "no flags"
    k = c.get("keys") or {}
    return f"{c['name']:45s} {k.get('main_names', ''):9s} {c.get('seconds', 0):5.1f}s  {fl}"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("sfz", nargs="*")
    ap.add_argument("--out", default=os.path.join(ROOT, "catalog"))
    ap.add_argument("--name", help="card name (one SFZ only)")
    ap.add_argument("--keys", help="scan only LO-HI, e.g. 36-96 or C2-C7")
    ap.add_argument("--ks", type=int, help="keyswitch key pressed before every note")
    ap.add_argument("--used", action="store_true", help="card every SFZ the songs use + the VCSL candidates")
    ap.add_argument("--index", action="store_true", help="only rebuild README.md from the JSON cards")
    ap.add_argument("-j", "--jobs", type=int, default=min(8, os.cpu_count() or 4))
    ap.add_argument("--worker", help=argparse.SUPPRESS)   # internal: measure one SFZ in this process
    a = ap.parse_args()
    sys.stdout.reconfigure(errors="replace")   # names with odd characters must not kill the run on a cp1252 console
    out = os.path.abspath(a.out)
    os.makedirs(out, exist_ok=True)
    lo, hi = (key_num(x) for x in re.split(r"(?<=\d)-", a.keys, maxsplit=1)) if a.keys else (0, 127)

    if a.worker:
        path = os.path.abspath(a.sfz[0])
        try:
            card = measure(path, a.name or ident(path), a.worker, lo, hi, a.ks)
        except Exception as e:   # anything unexpected still leaves a card behind
            card = dict(name=a.name or ident(path), slug=a.worker, sfz=os.path.relpath(path, ROOT).replace(os.sep, "/"),
                        measured=str(datetime.date.today()), error=f"{type(e).__name__}: {e}")
        card["flags"] = flags(card)
        write(card, os.path.join(out, a.worker + ".json"))
        return
    if a.index:
        index(out)
        return
    if a.name and len(a.sfz) != 1:
        ap.error("--name needs exactly one SFZ")
    if a.used:
        jobs = dedupe(gather_used())
    else:
        jobs = [dict(path=os.path.abspath(p), used_by=None, aliases=[]) for p in a.sfz]
    if not jobs:
        ap.error("give SFZ files or --used")
    for j in jobs:
        j["name"] = a.name or ident(j["path"])
        j["slug"] = slugify(j["name"] + ("" if not a.ks and not a.keys else f" ks{a.ks or ''} {a.keys or ''}"))
        j["keys"], j["ks"] = a.keys, a.ks
        old = os.path.join(out, j["slug"] + ".json")
        if j["used_by"] is None and os.path.isfile(old):   # keep the usage notes from an earlier --used run
            prev = json.load(open(old, encoding="utf-8"))
            j["used_by"], j["aliases"] = prev.get("used_by"), prev.get("aliases", [])
    print(f"carding {len(jobs)} instruments with {a.jobs} workers")
    t0 = time.time()
    with ThreadPoolExecutor(a.jobs) as ex:
        for fut in as_completed([ex.submit(run_job, j, out) for j in jobs]):
            print(summary(fut.result()), flush=True)
    index(out)
    print(f"done in {time.time() - t0:.0f} s -> {os.path.relpath(out, ROOT)}/README.md")


if __name__ == "__main__":
    main()
