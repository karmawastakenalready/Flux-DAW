"""Guitar/bass rigs: an SFZ source played by sfizz, then an insert chain (NAM amp captures, cab IRs, VST3 effects,
filters, gate). vst_render.py hands jobs whose plugin path ends in .rig.json to `render`.

Rig file (paths relative to the repo root):
    {"source": {"sfz": "instruments/.../program.sfz", "init_cc": {"22": 51}, "keyswitches": [[0, 29], [89, 127]],
                "tune_cents": -5, "rr_offset": 0, "default_ks": 17},
     "chain": [{"type": "highpass", "hz": 70},
               {"type": "nam", "model": "instruments/nam/.../x.nam", "input_db": 0, "output_db": 0},
               {"type": "ir", "path": "...wav", "mix": 1.0},
               {"type": "vst3", "path": "...vst3", "params": {}},
               {"type": "gate", "threshold_db": -55, "release_ms": 80},
               {"type": "peak", "hz": 2500, "db": -2, "q": 1.0}, {"type": "lowpass", "hz": 9000},
               {"type": "gain", "db": -6}]}

Job params override stage keys: {"nam.input_db": 3} sets input_db on the first stage whose "id" or "type" is nam.
"""
import json
import os
import sys

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))


def _path(p):
    return p if os.path.isabs(p) else os.path.join(ROOT, p)


def _nam_init():
    """NAM's model code, imported without the package __init__ (which pulls in the training stack)."""
    import importlib.util
    import types
    if "nam" not in sys.modules:
        spec = importlib.util.find_spec("nam")
        pkg = types.ModuleType("nam")
        pkg.__path__ = list(spec.submodule_search_locations)
        sys.modules["nam"] = pkg
    os.environ.setdefault("FOR_DISABLE_CONSOLE_CTRL_HANDLER", "1")
    from nam.models._from_nam import init_from_nam
    return init_from_nam


def _resample(x, sr_from, sr_to):
    if sr_from == sr_to:
        return x
    from scipy.signal import resample_poly
    g = np.gcd(int(sr_from), int(sr_to))
    return resample_poly(x, int(sr_to) // g, int(sr_from) // g).astype(np.float32)


def nam(x, sr, model, input_db=0.0, output_db=0.0, chunk_s=8.0):
    """Mono float32 through a .nam capture (A1 WaveNet/LSTM, or the largest member of an A2 container)."""
    import torch
    d = json.load(open(_path(model)))
    if d["architecture"] == "SlimmableContainer":
        d = max(d["config"]["submodels"], key=lambda s: s["max_value"])["model"]
    net = _nam_init()(d)
    net.eval()
    msr = int(d.get("sample_rate") or 48000)
    y = _resample(x * np.float32(10 ** (input_db / 20)), sr, msr)
    # chunked with a lead-in longer than the receptive field so chunk edges are seamless
    n, hop, ctx = len(y), int(chunk_s * msr), 16384
    out = np.zeros(n, dtype=np.float32)
    with torch.no_grad():
        for s in range(0, n, hop):
            a = max(0, s - ctx)
            seg = torch.tensor(y[a:min(n, s + hop)], dtype=torch.float32)[None]
            r = net(seg, pad_start=True).squeeze(0).numpy()
            out[s:min(n, s + hop)] = r[s - a:]
    out = _resample(out, msr, sr)[: len(x)]
    return out * np.float32(10 ** (output_db / 20))


def _stage(x, sr, st):
    import pedalboard as pb
    t = st["type"]
    if t == "nam":
        return nam(x, sr, st["model"], st.get("input_db", 0.0), st.get("output_db", 0.0))
    if t == "gain":
        return x * np.float32(10 ** (st["db"] / 20))
    fx = {
        "highpass": lambda: pb.HighpassFilter(cutoff_frequency_hz=st["hz"]),
        "lowpass": lambda: pb.LowpassFilter(cutoff_frequency_hz=st["hz"]),
        "peak": lambda: pb.PeakFilter(cutoff_frequency_hz=st["hz"], gain_db=st["db"], q=st.get("q", 1.0)),
        "lowshelf": lambda: pb.LowShelfFilter(cutoff_frequency_hz=st["hz"], gain_db=st["db"], q=st.get("q", 0.7)),
        "highshelf": lambda: pb.HighShelfFilter(cutoff_frequency_hz=st["hz"], gain_db=st["db"], q=st.get("q", 0.7)),
        "ir": lambda: pb.Convolution(_path(st["path"]), mix=st.get("mix", 1.0)),
        "gate": lambda: pb.NoiseGate(threshold_db=st.get("threshold_db", -55), ratio=st.get("ratio", 10),
                                     attack_ms=st.get("attack_ms", 1), release_ms=st.get("release_ms", 80)),
        "comp": lambda: pb.Compressor(threshold_db=st.get("threshold_db", -18), ratio=st.get("ratio", 3),
                                      attack_ms=st.get("attack_ms", 10), release_ms=st.get("release_ms", 120)),
    }
    if t == "vst3":
        plug = pb.load_plugin(_path(st["path"]))
        for k, v in (st.get("params") or {}).items():
            setattr(plug, k, v)
        y = plug(np.stack([x, x]), sr, reset=True)
        return np.asarray(y, dtype=np.float32).mean(0)
    if t not in fx:
        raise ValueError(f"unknown rig stage {t!r}")
    return np.asarray(fx[t]()(x[None], sr, reset=True), dtype=np.float32)[0]


def _round_robin_preroll(notes, is_ks, offset, default_ks, sr):
    """Silent lead-in that advances every round-robin counter the song uses by `offset` notes, so a double track
    on the same rig picks different samples than its twin. Returns (pre-roll length in samples, events)."""
    if offset <= 0:
        return 0, []
    switches = sorted({p for _, _, p, _ in notes if is_ks(p)}) or [None]
    pitches = sorted({p for _, _, p, _ in notes if not is_ks(p)})
    ev, t, step = [], 0, int(0.05 * sr)
    for k in switches:
        if k is not None:
            ev += [(t, "on", k, 100), (t + 10, "off", k, 0)]
            t += step
        for _ in range(offset):
            for p in pitches:
                ev += [(t, "on", p, 100), (t + step // 2, "off", p, 0)]
            t += step
    if default_ks is not None:
        ev += [(t, "on", default_ks, 100), (t + 10, "off", default_ks, 0)]
    return t + 4 * sr, ev   # four seconds for the release samples to die away


def render(job, rig_path):
    sys.path.insert(0, ROOT)
    from sfizz_engine import SfizzSynth
    rig = json.load(open(rig_path))
    chain = [dict(s) for s in rig.get("chain", [])]
    for key, v in (job.get("params") or {}).items():
        name, _, field = key.partition(".")
        st = next((s for s in chain if s.get("id") == name), None) or next((s for s in chain if s["type"] == name), None)
        if st is None or not field:
            raise ValueError(f"rig param {key!r} matches no stage")
        st[field] = v
    src = rig["source"]
    sr, frames = int(job["sample_rate"]), int(job["frames"])
    ks_ranges = src.get("keyswitches", [])
    is_ks = lambda p: any(lo <= p <= hi for lo, hi in ks_ranges)
    pre, warm = _round_robin_preroll(job["notes"], is_ks, int(src.get("rr_offset", 0)), src.get("default_ks"), sr)
    ev = [(0, "cc", int(c), int(v)) for c, v in (src.get("init_cc") or {}).items()] + warm
    for s, e, p, v in job["notes"]:
        # keyswitches go a sample early so a switch and a note at the same time land in that order
        ev.append((pre + max(0, round(s * sr) - (1 if is_ks(p) else 0)), "on", p, v))
        ev.append((pre + round(e * sr), "off", p, 0))
    ev += [(pre + round(t * sr), "cc", c, v) for t, c, v in job.get("ccs", [])]
    ev += [(pre + round(t * sr), "pb", v, 0) for t, v in job.get("bends", [])]
    syn = SfizzSynth(_path(src["sfz"]), sr)
    if src.get("tune_cents"):
        import ctypes
        from sfizz_engine import lib
        lib().sfizz_set_tuning_frequency.argtypes = [ctypes.c_void_p, ctypes.c_float]
        lib().sfizz_set_tuning_frequency(syn.h, 440.0 * 2 ** (src["tune_cents"] / 1200))
    try:
        y = syn.render(ev, pre + frames)[pre:]
    finally:
        syn.close()
    x = y.mean(1).astype(np.float32)
    for st in chain:
        x = _stage(x, sr, st)
    x = np.nan_to_num(x)[:frames]
    return np.stack([x, x])
