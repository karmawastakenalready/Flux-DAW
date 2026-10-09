"""Baton VST3 bridge: render MIDI through a VST3 instrument with pedalboard (offline, float32).

    python -I vst_render.py job.json out.wav

job.json: {"plugin": path, "params": {name: value}, "preset": path|null, "sample_rate": int, "frames": int,
           "notes": [[start_s, end_s, pitch, vel], ...], "ccs": [[t_s, cc, value], ...], "bends": [[t_s, value], ...]}
A plugin path ending in .rig.json is an SFZ + insert-chain rig rendered by rig.py instead of a VST3.
"""
import json
import os
import sys

import mido
import numpy as np
import soundfile as sf
from pedalboard import load_plugin


def main(job_path, out_path):
    job = json.load(open(job_path))
    if job["plugin"].endswith(".rig.json"):
        sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
        import rig
        audio = rig.render(job, job["plugin"])
        sf.write(out_path, audio.T, int(job["sample_rate"]), subtype="FLOAT")
        return
    plugin = load_plugin(job["plugin"])
    if job.get("preset"):
        plugin.load_preset(job["preset"])
    for k, v in (job.get("params") or {}).items():
        setattr(plugin, k, v)
    msgs = []
    for s, e, p, v in job["notes"]:
        msgs.append((s, mido.Message("note_on", note=int(p), velocity=int(v))))
        msgs.append((e, mido.Message("note_off", note=int(p), velocity=0)))
    for t, c, v in job.get("ccs", []):
        msgs.append((t, mido.Message("control_change", control=int(c), value=int(v))))
    for t, v in job.get("bends", []):
        msgs.append((t, mido.Message("pitchwheel", pitch=int(v))))
    msgs.sort(key=lambda m: (m[0], 0 if m[1].type in ("control_change", "pitchwheel") else 1 if m[1].type == "note_off" else 2))
    sr = int(job["sample_rate"])
    dur = job["frames"] / sr
    # raw MIDI bytes: newer pedalboard builds no longer accept mido.Message objects
    audio = plugin([(bytes(m.bytes()), t) for t, m in msgs], duration=dur, sample_rate=sr, num_channels=2, reset=True)
    audio = np.asarray(audio, dtype=np.float32)
    if audio.ndim == 1:
        audio = np.stack([audio, audio])
    sf.write(out_path, audio.T, sr, subtype="FLOAT")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
