"""master.py - mix the rendered stems and master them (44.1 kHz, float until the final 16-bit export).

    python master.py --meta output/capital_city/capital_city.json      # -> output/capital_city/capital_city_loop.wav/.flac
    python master.py --meta output/temporal_shift_battle/temporal_shift_battle.json

Per stem: high-pass, EQ carve, light compression, gain, stereo placement (pan + width), reverb send.
One shared reverb (pre-delay -> hall -> filtered return) so everything sits in the same room.
Master: gentle glue compression -> EQ -> loudness drive -> true-peak lookahead limiter -> TPDF dither.

Loop mode (sidecar JSON says loop=true): each raw stem's tail past the loop point is folded onto its
start, then the whole mix/master chain runs on three back-to-back copies and the middle copy is kept.
Every effect (compressors, reverb, limiter) is therefore in steady state at both ends, which makes
the seam seamless. No edge fades in loop mode (a fade would put a dip in every loop); one-shot
exports get 6 ms linear fades.

Time stops (sidecar JSON "timestops"): every stem not listed as frozen is tape-stopped (speed ramps
1 -> 0, pitch falls with it) between stop_s and freeze_s, then muted until resume_s. Frozen stems
(the held tremolo chord, the glockenspiel knives, the bowed cymbal, reversed swells) play on.
"""
import argparse
import glob
import json
import os

import numpy as np
import pyloudnorm as pyln
import soundfile as sf
from pedalboard import (Bitcrush, Chorus, Clipping, Compressor, Delay, Distortion, Gain, HighpassFilter,
                        HighShelfFilter, LowpassFilter, LowShelfFilter, Pedalboard, PeakFilter, Reverb)
from scipy.ndimage import minimum_filter1d, uniform_filter1d
from scipy.signal import resample_poly

ROOT = os.path.dirname(os.path.abspath(__file__))
SR = 44100

# ==================================================================================================
# The per-instrument MIX (gain, pan, width, high-pass, EQ, compression, reverb send) lives with each piece:
# pieces/<piece>.py -> MIX / mix_for(version). Shared room and master chain below.
# ==================================================================================================
REVERB = dict(predelay_s=0.025, room_size=0.86, damping=0.5, width=1.0, return_hpf=220, return_lpf=7500,
              return_gain_db=0.0)
MASTER = dict(
    glue=(-3.0, 1.8, 30.0, 250.0),          # (threshold relative to bus loud level dB, ratio, attack ms, release ms)
    eq=[("low_shelf", 110, -0.5, 0.7), ("peak", 320, -1.0, 0.8), ("peak", 650, -1.0, 0.9),
        ("high_shelf", 6000, 2.0, 0.7)],
    target_lufs=-15.0,                      # spec: -16 .. -14 LUFS integrated
    ceiling_dbtp=-1.0,                      # spec: true peak <= -1 dBTP
    limiter_window_ms=12.0,                 # lookahead/hold/release smoothing of the limiter
)
EDGE_FADE_MS = 6.0  # one-shot exports only


# ==================================================================================================
def _eq_plugins(eq):
    out = []
    for kind, hz, db, q in eq:
        if kind == "peak":
            out.append(PeakFilter(cutoff_frequency_hz=hz, gain_db=db, q=q))
        elif kind == "low_shelf":
            out.append(LowShelfFilter(cutoff_frequency_hz=hz, gain_db=db, q=q))
        elif kind == "high_shelf":
            out.append(HighShelfFilter(cutoff_frequency_hz=hz, gain_db=db, q=q))
    return out


def _fx_plugins(fx, loop_s=None):
    """Per-stem creative effects (applied after HPF/EQ/gain): distortion, lowpass, chorus, delay, bitcrush, clip.
    In a loop, LFO rates are snapped to a whole number of cycles per loop so the modulation is seamless too."""
    out = []
    for item in fx:
        kind = item[0]
        if kind == "distortion":
            out.append(Distortion(drive_db=item[1]))
        elif kind == "lowpass":
            out.append(LowpassFilter(cutoff_frequency_hz=item[1]))
        elif kind == "chorus":
            rate = item[1] if not loop_s else max(1, round(item[1] * loop_s)) / loop_s
            out.append(Chorus(rate_hz=rate, depth=item[2], centre_delay_ms=7.0, feedback=0.0, mix=item[3]))
        elif kind == "delay":
            out.append(Delay(delay_seconds=item[1], feedback=item[2], mix=item[3]))
        elif kind == "bitcrush":
            out.append(Bitcrush(bit_depth=item[1]))
        elif kind == "clip":
            out.append(Clipping(threshold_db=item[1]))
    return out


def duck_envelope(n, triggers_s, depth_db, release_ms, loop_len=None):
    """Sidechain-style gain envelope: dips by depth_db at each trigger (5 ms attack) and recovers exponentially.
    Triggers are also wrapped by one loop length so the envelope is continuous across the loop seam."""
    t = np.arange(n) / SR
    d = np.zeros(n)
    tau = release_ms / 1000.0
    trig = list(triggers_s) + ([x - loop_len for x in triggers_s] if loop_len else [])
    att = 0.005
    for tk in trig:
        a = max(0, int((tk - att) * SR))
        b = min(n, int((tk + 6 * tau) * SR))
        if b <= 0 or a >= n:
            continue
        tt = t[a:b] - tk
        env = np.where(tt < 0, depth_db * (1 + tt / att), depth_db * np.exp(-tt / tau))
        d[a:b] = np.maximum(d[a:b], env)
    return (10 ** (-d / 20)).astype(np.float32)


def loud_level_db(x, win_s=0.4, pct=95):
    """95th percentile of short-term RMS (dBFS) over non-silent windows: 'how loud it gets'."""
    m = x.mean(axis=1) if x.ndim == 2 else x
    w = int(win_s * SR)
    n = len(m) // w
    if n == 0:
        return -100.0
    r = np.sqrt((m[: n * w].reshape(n, w) ** 2).mean(axis=1))
    db = 20 * np.log10(r + 1e-12)
    db = db[db > -70]
    return float(np.percentile(db, pct)) if len(db) else -100.0


def place(x, pan, width):
    """Stereo stem -> width (mid/side) then constant-power balance."""
    mid = (x[:, 0] + x[:, 1]) * 0.5
    side = (x[:, 0] - x[:, 1]) * 0.5 * width
    left, right = mid + side, mid - side
    th = (pan + 1) * np.pi / 4
    return np.stack([left * np.sqrt(2) * np.cos(th), right * np.sqrt(2) * np.sin(th)], axis=1).astype(np.float32)


def process(board, x):
    """pedalboard works on (channels, samples)."""
    return board(x.T.copy(), SR).T.astype(np.float32)


def true_peak(x, os_factor=4):
    up = resample_poly(x, os_factor, 1, axis=0)
    return 20 * np.log10(np.abs(up).max() + 1e-12)


def tp_limiter(x, ceiling_db, window_ms):
    """Lookahead true-peak limiter: detection on 4x-oversampled linked peaks, gain = min-filter
    (lookahead + hold) followed by a moving average of the same length. Because every sample inside
    the averaging window has a min-window that contains the peak, the smoothed gain never exceeds
    the gain the peak requires -> ceiling is respected without clipping."""
    ceil = 10 ** (ceiling_db / 20)
    up = np.abs(resample_poly(x, 4, 1, axis=0)).max(axis=1)
    n = len(x)
    up = up[: n * 4].reshape(n, 4).max(axis=1)
    g_req = np.minimum(1.0, ceil / np.maximum(up, 1e-12))
    size = max(3, int(window_ms / 1000 * SR) | 1)
    g = minimum_filter1d(g_req, size=size, mode="nearest")
    g = uniform_filter1d(g, size=size, mode="nearest")
    g = np.minimum(g, minimum_filter1d(g_req, size=size, mode="nearest"))
    return (x * g[:, None]).astype(np.float32), float(20 * np.log10(g.min()))


def tape_stop(y, a, b, r):
    """Tape-stop between samples a..b (playback speed falls linearly 1 -> 0, so pitch falls with it and only
    half of the source window is consumed), silence until r (minus 30 ms for early-played attacks)."""
    out = y.copy()
    D = b - a
    if D > 0:
        tau = np.arange(D, dtype=np.float64)
        src = a + tau - tau ** 2 / (2 * D)
        idx = np.arange(len(y))
        seg = np.stack([np.interp(src, idx, y[:, c]) for c in range(y.shape[1])], axis=1)
        env = np.ones(D)
        nf = int(0.4 * D)
        env[-nf:] = np.linspace(1.0, 0.0, nf) ** 1.5
        out[a:b] = (seg * env[:, None]).astype(np.float32)
    g = max(b, r - int(0.03 * SR))
    out[b:g] = 0.0
    return out


def tpdf_to_int16(x, rng):
    """TPDF dither (+/-1 LSB triangular) applied once, at the final float -> 16-bit conversion."""
    scaled = x.astype(np.float64) * 32767.0
    d = rng.random(scaled.shape) - rng.random(scaled.shape)
    return np.clip(np.round(scaled + d), -32768, 32767).astype(np.int16)


# ==================================================================================================
def mix_and_master(stem_dir, meta, out_base, seed=0):
    from compose import load_piece
    piece_mod = load_piece(meta.get("piece", "capital_city"))
    MIX = piece_mod.mix_for(meta.get("version"))
    loop = bool(meta.get("loop"))
    stems = {}
    for f in sorted(glob.glob(os.path.join(stem_dir, "*.wav"))):
        name = os.path.splitext(os.path.basename(f))[0]
        if name not in MIX:
            continue
        x, sr = sf.read(f, dtype="float32", always_2d=True)
        assert sr == SR, f"{f}: {sr} Hz"
        if np.abs(x).max() < 1e-7:      # unused instrument in this arrangement
            continue
        stems[name] = x
    n_total = max(len(x) for x in stems.values())
    L = int(round(meta["loop_length_s"] * SR)) if loop else n_total
    reps = 3 if loop else 1

    def prep(x, name):
        x = np.pad(x, ((0, max(0, n_total - len(x))), (0, 0)))
        if MIX[name].get("duck") and meta.get("duck_triggers"):
            depth, rel = MIX[name]["duck"]
            x = x * duck_envelope(len(x), meta["duck_triggers"], depth, rel, L if loop else None)[:, None]
        for ts in meta.get("timestops", []):
            if name not in ts["frozen_parts"]:
                x = tape_stop(x, int(round(ts["stop_s"] * SR)), int(round(ts["freeze_s"] * SR)),
                              int(round(ts["resume_s"] * SR)))
        if not loop:
            return x
        y = x[:L].copy()
        tail = x[L:]
        k = 0
        while k < len(tail):                      # fold the tail (reverb/decays past the loop point)
            seg = tail[k:k + L]
            y[: len(seg)] += seg
            k += L
        return np.tile(y, (reps, 1))

    dry = None
    send = None
    stem_report = {}
    for name, x in stems.items():
        cfg = MIX[name]
        x = prep(x, name)
        pre = Pedalboard([HighpassFilter(cutoff_frequency_hz=cfg["hpf"]), *_eq_plugins(cfg["eq"]), Gain(gain_db=cfg["gain"])])
        y = process(pre, x)
        if cfg.get("fx"):
            y = process(Pedalboard([*_fx_plugins(cfg["fx"], L / SR if loop else None),
                                    Gain(gain_db=cfg.get("post_gain", 0.0))]), y)
        if cfg["comp"]:
            thr = loud_level_db(y) + cfg["comp"][0]
            y = process(Pedalboard([Compressor(threshold_db=thr, ratio=cfg["comp"][1], attack_ms=15, release_ms=150)]), y)
        y = place(y, cfg["pan"], cfg["width"])
        dry = y if dry is None else dry + y
        send = y * cfg["send"] if send is None else send + y * cfg["send"]
        stem_report[name] = dict(loud_db=round(loud_level_db(y), 1))

    R = getattr(piece_mod, "REVERB", REVERB)
    verb = Pedalboard([
        Delay(delay_seconds=R["predelay_s"], feedback=0.0, mix=1.0),
        Reverb(room_size=R["room_size"], damping=R["damping"], wet_level=1.0, dry_level=0.0, width=R["width"]),
        HighpassFilter(cutoff_frequency_hz=R["return_hpf"]),
        LowpassFilter(cutoff_frequency_hz=R["return_lpf"]),
        Gain(gain_db=R["return_gain_db"]),
    ])
    bus = dry + process(verb, send)

    M = MASTER
    g_thr = loud_level_db(bus) + M["glue"][0]
    bus = process(Pedalboard([Compressor(threshold_db=g_thr, ratio=M["glue"][1], attack_ms=M["glue"][2],
                                         release_ms=M["glue"][3]), *_eq_plugins(M["eq"])]), bus)

    meter = pyln.Meter(SR)

    def middle(z):
        return z[L:2 * L] if loop else z

    drive = M["target_lufs"] - meter.integrated_loudness(middle(bus))
    ceiling = M["ceiling_dbtp"] - 0.15     # small safety margin for the reconstruction filter
    for _ in range(8):
        lim, gr = tp_limiter(bus * 10 ** (drive / 20), ceiling, M["limiter_window_ms"])
        out = middle(lim)
        lufs = meter.integrated_loudness(out)
        tp = true_peak(out)
        if abs(lufs - M["target_lufs"]) < 0.1 and tp <= M["ceiling_dbtp"]:
            break
        drive += M["target_lufs"] - lufs
        if tp > M["ceiling_dbtp"]:
            ceiling -= (tp - M["ceiling_dbtp"]) + 0.05

    seam_err_db = None
    if loop:
        # proof of seamlessness: the start of the kept (middle) copy must equal what really follows its
        # end in continuous playback (= start of the third copy). Identical -> the wrap-around is seamless.
        w = int(0.5 * SR)
        diff = np.abs(lim[2 * L:2 * L + w] - lim[L:L + w]).max()
        seam_err_db = float(20 * np.log10(diff + 1e-12))

    if not loop:  # trim trailing silence (keep 0.5 s after the tail falls below -70 dBFS), fade edges
        env = np.abs(out).max(axis=1)
        last = np.nonzero(env > 10 ** (-70 / 20))[0]
        end = min(len(out), (last[-1] if len(last) else len(out)) + int(0.5 * SR))
        out = out[:end].copy()
        nf = int(EDGE_FADE_MS / 1000 * SR)
        ramp = np.linspace(0.0, 1.0, nf, dtype=np.float32)[:, None]
        out[:nf] *= ramp
        out[-nf:] *= ramp[::-1]

    rng = np.random.default_rng(seed)
    pcm = tpdf_to_int16(out, rng)
    os.makedirs(os.path.dirname(out_base), exist_ok=True)
    wav_path, flac_path = out_base + ".wav", out_base + ".flac"
    sf.write(wav_path, pcm, SR, subtype="PCM_16")
    sf.write(flac_path, pcm, SR, subtype="PCM_16", format="FLAC")
    report = dict(
        loop=loop, loop_samples=int(L) if loop else None, samples=int(len(pcm)), duration_s=round(len(pcm) / SR, 3),
        drive_db=round(float(drive), 2), limiter_max_gr_db=round(float(gr), 2), glue_threshold_db=round(float(g_thr), 1),
        lufs_float=round(float(lufs), 2), true_peak_float_dbtp=round(float(tp), 2), stems=stem_report,
        seam_steady_state_error_dbfs=None if seam_err_db is None else round(seam_err_db, 1),
        wav=wav_path, flac=flac_path,
    )
    with open(out_base + "_master_report.json", "w") as f:
        json.dump(report, f, indent=2)
    return report


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--meta", default=os.path.join(ROOT, "output", "capital_city", "capital_city.json"))
    ap.add_argument("--stems", default=None, help="default: <meta folder>/stems")
    ap.add_argument("--out", default=None, help="output path without extension")
    ap.add_argument("--target-lufs", type=float, default=MASTER["target_lufs"])
    ap.add_argument("--ceiling-dbtp", type=float, default=MASTER["ceiling_dbtp"])
    a = ap.parse_args()
    MASTER["target_lufs"], MASTER["ceiling_dbtp"] = a.target_lufs, a.ceiling_dbtp
    meta = json.load(open(a.meta))
    folder = os.path.dirname(os.path.abspath(a.meta))
    out = a.out or os.path.join(folder, meta["title"] + ("_loop" if meta.get("loop") else ""))
    r = mix_and_master(a.stems or os.path.join(folder, "stems"), meta, out, seed=meta.get("seed", 0))
    print(f"  mastered -> {r['wav']}  ({r['duration_s']} s)  LUFS {r['lufs_float']}  TP {r['true_peak_float_dbtp']} dBTP  "
          f"drive {r['drive_db']} dB  limiter max GR {r['limiter_max_gr_db']} dB")


if __name__ == "__main__":
    main()
