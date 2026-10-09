"""Render each instrument of a composed MIDI to its own float32 stereo stem (44.1 kHz).

    python render.py --test --set temporal_shift               # test notes for every instrument/articulation
    python render.py --midi output/capital_city/capital_city.mid  # render all stems of a composition

Engine: sfizz (C API via ctypes, float32). Fallback for any instrument that fails to load or
renders silent: FluidSynth + GeneralUser GS (float output). Tracks whose instrument has reverse=True
are rendered note by note and time-reversed, so each note's attack lands on the note's END.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile

import numpy as np
import pretty_midi
import soundfile as sf

from instruments import GENERALUSER_SF2, INSTRUMENT_SETS, KS_BASE, ROOT, prepare_sfz
from sfizz_engine import SfizzSynth, midi_track_to_events

SR = 44100
TAIL_S = 6.0  # extra render time so reverb/decays are not chopped
FLUIDSYNTH = shutil.which("fluidsynth") or os.path.join(
    ROOT, "tools", "fluidsynth", "fluidsynth-v2.6.1-win10-x64-cpp11", "bin", "fluidsynth.exe")


def db(x):
    return 20 * np.log10(max(float(x), 1e-12))


# ---------------------------------------------------------------- engines
def render_sfizz(sfz_path, notes, ccs, bends, total_s):
    synth = SfizzSynth(sfz_path, SR)
    try:
        ev = midi_track_to_events(notes, ccs, SR, bends)
        return synth.render(ev, int(total_s * SR))
    finally:
        synth.close()


def render_reverse(sfz_path, notes, total_s):
    """Each note rendered alone for its own length, reversed, and placed so the attack hits the note end."""
    out = np.zeros((int(total_s * SR), 2), np.float32)
    for s, e, p, v in notes:
        n = int(round((e - s) * SR))
        if n <= 0:
            continue
        x = render_sfizz(sfz_path, [(0.0, (e - s) + 0.2, p, v)], [], [], (e - s) + 0.3)[:n][::-1].copy()
        fade = min(len(x), int(0.02 * SR))
        x[:fade] *= np.linspace(0, 1, fade, dtype=np.float32)[:, None]
        a = int(round(s * SR))
        b = min(len(out), a + len(x))
        out[a:b] += x[: b - a]
    return out


def render_fluidsynth(gm, notes, ccs, bends, total_s, is_drum=False, ks_pitches=()):
    if not (os.path.exists(FLUIDSYNTH) and os.path.exists(GENERALUSER_SF2)):
        raise RuntimeError("FluidSynth or GeneralUser GS missing")
    bank, program = gm
    pm = pretty_midi.PrettyMIDI(initial_tempo=120)
    inst = pretty_midi.Instrument(program=program if bank != 128 else 0, is_drum=is_drum or bank == 128)
    for s, e, p, v in notes:
        if p >= KS_BASE or (p in ks_pitches and e - s <= 0.03):   # keyswitches mean nothing to a GM patch
            continue
        inst.notes.append(pretty_midi.Note(velocity=int(v), pitch=int(p), start=s, end=e))
    for t, c, val in ccs:
        inst.control_changes.append(pretty_midi.ControlChange(number=int(c), value=int(val), time=t))
    for t, val in bends:
        inst.pitch_bends.append(pretty_midi.PitchBend(pitch=int(val), time=t))
    inst.control_changes.append(pretty_midi.ControlChange(number=110, value=0, time=total_s))
    pm.instruments.append(inst)
    with tempfile.TemporaryDirectory() as td:
        mid = os.path.join(td, "x.mid")
        wav = os.path.join(td, "x.wav")
        pm.write(mid)
        cmd = [FLUIDSYNTH, "-ni", "-q", "-F", wav, "-T", "wav", "-O", "float", "-r", str(SR), "-g", "0.6",
               "-o", "synth.reverb.active=0", "-o", "synth.chorus.active=0", GENERALUSER_SF2, mid]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode or not os.path.exists(wav):
            raise RuntimeError(f"fluidsynth failed: {r.stderr.strip()[:300]}")
        x, sr = sf.read(wav, dtype="float32", always_2d=True)
    assert sr == SR
    n = int(total_s * SR)
    if len(x) < n:
        x = np.vstack([x, np.zeros((n - len(x), x.shape[1]), np.float32)])
    return x[:n]


def render_part(name, spec, sfz_path, notes, ccs, bends, total_s, force_fallback=False):
    if not force_fallback:
        try:
            if spec.get("reverse"):
                x = render_reverse(sfz_path, notes, total_s)
            else:
                x = render_sfizz(sfz_path, notes, ccs, bends, total_s)
            if np.abs(x).max() > 1e-5 or not [n for n in notes if n[2] < KS_BASE]:
                return x, "sfizz"
            print(f"  ! {name}: sfizz rendered silence, falling back to GeneralUser GS")
        except Exception as exc:  # noqa: BLE001
            print(f"  ! {name}: sfizz failed ({exc}), falling back to GeneralUser GS")
    x = render_fluidsynth(spec["gm"], notes, ccs, bends, total_s, is_drum=spec["gm"][0] == 128,
                          ks_pitches=tuple(spec.get("ks_map", {}).values()))
    return (x[::-1].copy() if spec.get("reverse") else x), "fluidsynth"


# ---------------------------------------------------------------- test gate
def run_test(set_name):
    instruments = INSTRUMENT_SETS[set_name]
    paths = prepare_sfz(set_name)
    os.makedirs(os.path.join(ROOT, "build", "test_notes", set_name), exist_ok=True)
    rows, ok_all = [], True
    jobs = []
    for name, spec in instruments.items():
        arts = [a[0] for a in spec["arts"]] if "arts" in spec else [None]
        for k, art in enumerate(arts):
            for note in spec.get("test_notes", (spec["test_note"],)):
                jobs.append((name, art, k, note, False))
    first = next(iter(instruments))
    jobs.append((first, None, 0, instruments[first]["test_note"], True))  # prove the GeneralUser GS fallback
    for name, art, k, note, fb in jobs:
        spec = instruments[name]
        notes = [(0.05, 1.6, note, 96)]
        if art is not None and k:
            notes = [(0.0, 0.02, KS_BASE + k, 100)] + notes
        ccs = [(0.0, 1, 90), (0.0, 11, 110)]
        try:
            x, eng = render_part(name, spec, paths[name], notes, ccs, [], 4.0, force_fallback=fb)
            peak = float(np.abs(x).max())
            w = int(0.05 * SR)
            mono = x.mean(axis=1)
            rms = float(max(np.sqrt(np.mean(mono[i:i + w] ** 2)) for i in range(0, len(mono) - w, w // 2)))
            clipped = int(np.sum(np.abs(x) >= 1.0))
            ok = peak > 10 ** (-50 / 20) and rms > 10 ** (-60 / 20) and clipped == 0 and peak < 1.0
            tag = f"{name}{'_' + art if art else ''}_{note}{'_gm' if fb else ''}"
            sf.write(os.path.join(ROOT, "build", "test_notes", set_name, tag + ".wav"), x, SR, subtype="FLOAT")
        except Exception as exc:  # noqa: BLE001
            eng, peak, rms, clipped, ok = f"ERROR {exc}", 0, 0, 0, False
        ok_all &= ok
        label = name + (f" [{art}]" if art else "") + (" (GM fallback)" if fb else "")
        rows.append(f"{label:26s} note {note:3d}  {eng:10s} peak {db(peak):6.1f} dBFS  rms {db(rms):6.1f} dBFS  "
                    f"clipped {clipped:3d}  {'PASS' if ok else 'FAIL'}")
    print("\n".join(rows))
    print("ALL INSTRUMENTS PASS" if ok_all else "SOME INSTRUMENTS FAILED")
    return ok_all


# ---------------------------------------------------------------- full render
def run_render(midi_path, out_dir):
    meta_path = os.path.splitext(midi_path)[0] + ".json"
    meta = json.load(open(meta_path)) if os.path.exists(meta_path) else {}
    set_name = meta.get("instrument_set", "capital_city")
    instruments = INSTRUMENT_SETS[set_name]
    paths = prepare_sfz(set_name)
    pm = pretty_midi.PrettyMIDI(midi_path)
    music_end = meta.get("loop_length_s") or pm.get_end_time()
    total_s = music_end + TAIL_S
    os.makedirs(out_dir, exist_ok=True)
    report = {}
    for inst in pm.instruments:
        name = inst.name
        if name not in instruments:
            print(f"  skipping unknown track {name!r}")
            continue
        notes = [(n.start, n.end, n.pitch, n.velocity) for n in inst.notes]
        ccs = [(c.time, c.number, c.value) for c in inst.control_changes]
        bends = [(b.time, b.pitch) for b in inst.pitch_bends]
        x, eng = render_part(name, instruments[name], paths[name], notes, ccs, bends, total_s)
        x = x.astype(np.float32)
        sf.write(os.path.join(out_dir, f"{name}.wav"), x, SR, subtype="FLOAT")
        sounding = sum(1 for n in notes if n[2] < KS_BASE)
        report[name] = dict(engine=eng, notes=sounding, peak_dbfs=round(db(np.abs(x).max()), 2))
        print(f"  {name:11s} {eng:10s} notes={sounding:4d} peak={db(np.abs(x).max()):6.1f} dBFS")
    with open(os.path.join(out_dir, "render_report.json"), "w") as f:
        json.dump(dict(total_s=total_s, music_end_s=music_end, tail_s=TAIL_S, stems=report), f, indent=2)
    return report


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--test", action="store_true", help="render one test note per instrument and exit")
    ap.add_argument("--set", default="capital_city", help="instrument set for --test")
    ap.add_argument("--midi", default=os.path.join(ROOT, "output", "capital_city", "capital_city.mid"))
    ap.add_argument("--stems", default=None, help="default: <midi folder>/stems")
    a = ap.parse_args()
    if a.test:
        sys.exit(0 if run_test(a.set) else 1)
    run_render(a.midi, a.stems or os.path.join(os.path.dirname(os.path.abspath(a.midi)), "stems"))


if __name__ == "__main__":
    main()
