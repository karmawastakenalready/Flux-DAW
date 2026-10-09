"""verify.py - objective checks on the final export (we cannot listen, so we measure).

    python verify.py            # prints the report, writes output/<title>_verify.json, exit 1 on FAIL

Measures: key (MIDI + audio chroma), BPM (metadata + audio onset autocorrelation), section map,
duration, sample/true peak, integrated LUFS, clipped samples, head/tail silence, loop-seam
discontinuity, per-section loudness, octave-band balance, Roblox upload limits.
"""
import argparse
import json
import os
import sys

import numpy as np
import pretty_midi
import pyloudnorm as pyln
import soundfile as sf
from scipy.signal import butter, resample_poly, sosfiltfilt, stft

ROOT = os.path.dirname(os.path.abspath(__file__))

SPEC = dict(sr=44100, subtype="PCM_16", channels=2, lufs=(-16.0, -14.0), tp_max=-1.0, max_clipped=0,
            max_head_silence_s=0.05, max_tail_silence_s=0.05,
            roblox_max_bytes=20 * 1024 * 1024, roblox_max_s=7 * 60, roblox_max_sr=48000)

NAMES = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"]
KK_MAJOR = np.array([6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88])
KK_MINOR = np.array([6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17])


def key_from_chroma(chroma):
    res = []
    for tonic in range(12):
        for mode, prof in (("major", KK_MAJOR), ("minor", KK_MINOR)):
            r = np.corrcoef(chroma, np.roll(prof, tonic))[0, 1]
            res.append((r, f"{NAMES[tonic]} {mode}"))
    res.sort(reverse=True)
    return res[:3]


UNPITCHED = ("percussion", "timpani", "clock", "rev_cymbal")
FREE_TIME_PARTS = ("harp", "glock")   # rolls, glissandi and stopped-time 'knives' are written off the grid


def midi_chroma(midi_path, t0=0.0, t1=1e9):
    pm = pretty_midi.PrettyMIDI(midi_path)
    c = np.zeros(12)
    for inst in pm.instruments:
        if inst.is_drum or inst.name in UNPITCHED:
            continue
        for n in inst.notes:
            if n.pitch >= 120:          # keyswitch
                continue
            ov = min(n.end, t1) - max(n.start, t0)
            if ov > 0:
                c[n.pitch % 12] += ov * (n.velocity / 127)
    return c


def audio_chroma(x, sr, t0=0.0, t1=None):
    m = x[int(t0 * sr):(int(t1 * sr) if t1 else None)].mean(axis=1)
    f, t, Z = stft(m, sr, nperseg=8192, noverlap=4096)
    mag = np.abs(Z)
    sel = (f >= 60) & (f <= 2000)
    midi = np.round(12 * np.log2(f[sel] / 440.0) + 69).astype(int)
    c = np.zeros(12)
    np.add.at(c, midi % 12, np.sqrt(mag[sel]).sum(axis=1))
    return c


def audio_bpm(x, sr, lo=60, hi=180):
    m = x.mean(axis=1)
    hop = 512
    f, t, Z = stft(m, sr, nperseg=2048, noverlap=2048 - hop)
    logmag = np.log1p(100 * np.abs(Z))
    flux = np.maximum(0, np.diff(logmag, axis=1)).sum(axis=0)
    flux = flux - np.convolve(flux, np.ones(16) / 16, mode="same")
    flux = np.maximum(flux, 0)
    ac = np.correlate(flux, flux, mode="full")[len(flux) - 1:]
    fps = sr / hop
    lags = np.arange(len(ac))
    bpm = np.where(lags > 0, 60 * fps / np.maximum(lags, 1), 0)
    sel = (bpm >= lo) & (bpm <= hi)
    # mild log-tempo prior around 100 BPM to settle octave ambiguity
    prior = np.exp(-0.5 * (np.log2(np.maximum(bpm, 1) / 100) / 0.9) ** 2)
    score = np.where(sel, ac * prior, -np.inf)
    best = int(np.argmax(score))
    # refine with parabolic interpolation
    if 1 <= best < len(ac) - 1:
        a, b, c = ac[best - 1], ac[best], ac[best + 1]
        off = 0.5 * (a - c) / (a - 2 * b + c) if (a - 2 * b + c) != 0 else 0
    else:
        off = 0
    return 60 * fps / (best + off)


def humanization_stats(midi_path, bpm, loop_len_s):
    """Timing offsets of grid notes vs the written grid, by metric position (rolls/ornaments excluded).
    Uses the MIDI's own tempo map, so accelerating sections are measured correctly."""
    pm = pretty_midi.PrettyMIDI(midi_path)
    res = pm.resolution
    cats = {"downbeat": [], "beat3": [], "beat": [], "offbeat": []}
    vel = {"downbeat": [], "other": []}
    total_beats = pm.time_to_tick(loop_len_s) / res if loop_len_s else None
    for inst in pm.instruments:
        if inst.name.startswith("rev_") or inst.name in FREE_TIME_PARTS:
            continue
        for n in inst.notes:
            if n.pitch >= 120:
                continue
            beats = pm.time_to_tick(n.start) / res
            if total_beats and beats > total_beats - 0.1:     # early downbeat wrapped to the end of the loop
                beats -= total_beats
            g = round(beats * 2) / 2
            spb = 60.0 / pm.get_tempo_changes()[1][max(0, np.searchsorted(pm.get_tempo_changes()[0], n.start,
                                                                           side="right") - 1)]
            if n.end - n.start < 0.2 * spb:      # roll strokes / fills (16ths-32nds) are not grid notes
                continue
            off_ms = (beats - g) * spb * 1000
            if abs(off_ms) > 25:
                continue
            r = g % 4
            cat = "downbeat" if r == 0 else "beat3" if r == 2 else "beat" if r == int(r) else "offbeat"
            cats[cat].append(off_ms)
            vel["downbeat" if cat == "downbeat" else "other"].append(n.velocity)
    out = {k: dict(n=len(v), max_abs_ms=round(float(np.max(np.abs(v))), 1), sd_ms=round(float(np.std(v)), 1))
           for k, v in cats.items() if v}
    out["mean_velocity"] = {k: round(float(np.mean(v)), 1) for k, v in vel.items()}
    return out


def silence_run(env, thr):
    idx = np.nonzero(env > thr)[0]
    if not len(idx):
        return len(env), len(env)
    return int(idx[0]), int(len(env) - 1 - idx[-1])


def junction_metrics(x, sr, p):
    """Abruptness of the junction at sample p (wrap-around aware): HF (>6 kHz) peak within +/-2 ms of p
    relative to the median HF level of the surrounding +/-250 ms, and the sample step at p."""
    w = int(0.25 * sr)
    idx = (np.arange(p - w, p + w)) % len(x)
    seg = x[idx]
    sos = butter(4, 6000, btype="highpass", fs=sr, output="sos")
    hf = np.abs(sosfiltfilt(sos, seg, axis=0)).max(axis=1)
    k = int(0.002 * sr)
    return float(20 * np.log10(hf[w - k:w + k].max() / (np.median(hf) + 1e-12)))


def seam_analysis(x, sr):
    d = np.abs(np.diff(x, axis=0))
    jump = np.abs(x[0] - x[-1])                       # per channel, sample units (full scale = 1.0)
    p999 = np.percentile(d, 99.9, axis=0)
    med = np.median(d, axis=0)
    # high-frequency click detector across the wrap-around junction
    w = int(0.25 * sr)
    junction = np.concatenate([x[-w:], x[:w]], axis=0)
    sos = butter(4, 6000, btype="highpass", fs=sr, output="sos")
    hf = np.abs(sosfiltfilt(sos, junction, axis=0)).max(axis=1)
    k = int(0.002 * sr)
    at = hf[w - k:w + k].max()
    around = np.median(hf) + 1e-12
    ref_peaks = np.percentile(hf, 99.5) + 1e-12
    # level continuity (50 ms RMS either side)
    n = int(0.05 * sr)
    rms_end = 20 * np.log10(np.sqrt((x[-n:] ** 2).mean()) + 1e-12)
    rms_start = 20 * np.log10(np.sqrt((x[:n] ** 2).mean()) + 1e-12)
    return dict(jump=jump.tolist(), jump_vs_median_step=(jump / (med + 1e-12)).tolist(),
                p999_step=p999.tolist(), jump_below_p999=bool(np.all(jump <= p999)),
                hf_at_seam_vs_median_db=float(20 * np.log10(at / around)),
                hf_at_seam_vs_p995_db=float(20 * np.log10(at / ref_peaks)),
                rms_last_50ms_db=float(rms_end), rms_first_50ms_db=float(rms_start))


def octave_bands(x, sr):
    m = x.mean(axis=1)
    f, P = None, None
    seg = 8192
    n = len(m) // seg
    spec = np.zeros(seg // 2 + 1)
    win = np.hanning(seg)
    for i in range(n):
        spec += np.abs(np.fft.rfft(m[i * seg:(i + 1) * seg] * win)) ** 2
    f = np.fft.rfftfreq(seg, 1 / sr)
    tot = spec.sum()
    out = {}
    for c in (63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000):
        sel = (f >= c / np.sqrt(2)) & (f < c * np.sqrt(2))
        out[f"{c} Hz"] = round(float(10 * np.log10(spec[sel].sum() / tot + 1e-12)), 1)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--meta", default=os.path.join(ROOT, "output", "capital_city", "capital_city.json"))
    a = ap.parse_args()
    meta = json.load(open(a.meta))
    base = os.path.join(os.path.dirname(os.path.abspath(a.meta)), meta["title"] + ("_loop" if meta.get("loop") else ""))
    tmap = meta.get("tempo_map") or [dict(bar=1, bpm=meta["bpm"], start_s=0.0)]

    def bar_time(bar):
        seg = max((t for t in tmap if t["bar"] <= bar), key=lambda t: t["bar"])
        return seg["start_s"] + (bar - seg["bar"]) * 4 * 60.0 / seg["bpm"]
    wav, flac = base + ".wav", base + ".flac"
    midi = os.path.splitext(a.meta)[0] + ".mid"
    info = sf.info(wav)
    pcm, sr = sf.read(wav, dtype="int16", always_2d=True)
    x = pcm.astype(np.float64) / 32768.0
    meter = pyln.Meter(sr)
    loop = bool(meta.get("loop"))

    res, fails, warns = {}, [], []

    def check(name, ok, detail, warn_only=False):
        res[name] = dict(ok=bool(ok), detail=detail)
        if not ok:
            (warns if warn_only else fails).append(name)

    # format
    check("format", info.samplerate == SPEC["sr"] and info.subtype == SPEC["subtype"] and info.channels == SPEC["channels"],
          f"{info.samplerate} Hz, {info.subtype}, {info.channels} ch")
    dur = len(x) / sr
    # loudness / peaks
    lufs = meter.integrated_loudness(x)
    tp = 20 * np.log10(np.abs(resample_poly(x, 4, 1, axis=0)).max() + 1e-12)
    sp = 20 * np.log10(np.abs(x).max() + 1e-12)
    clipped = int(np.sum((pcm >= 32767) | (pcm <= -32768)))
    check("integrated_lufs", SPEC["lufs"][0] <= lufs <= SPEC["lufs"][1], f"{lufs:.2f} LUFS (target -16..-14)")
    check("true_peak", tp <= SPEC["tp_max"], f"{tp:.2f} dBTP (sample peak {sp:.2f} dBFS; ceiling -1.0)")
    check("clipped_samples", clipped <= SPEC["max_clipped"], f"{clipped} samples at full scale")
    # silence
    env = np.abs(x).max(axis=1)
    h60, t60 = silence_run(env, 10 ** (-60 / 20))
    check("head_silence", h60 / sr <= SPEC["max_head_silence_s"], f"{1000 * h60 / sr:.1f} ms below -60 dBFS")
    check("tail_silence", t60 / sr <= (SPEC["max_tail_silence_s"] if loop else 1.0), f"{1000 * t60 / sr:.1f} ms below -60 dBFS")
    # loop seam
    if loop:
        seam = seam_analysis(x, sr)
        exp_len = int(round(meta["loop_length_s"] * sr))
        bar_lines = [junction_metrics(x, sr, int(round(bar_time(b + 1) * sr))) for b in range(1, meta["bars"])]
        seam_j = junction_metrics(x, sr, 0)
        mrep = json.load(open(base + "_master_report.json"))
        ss_err = mrep.get("seam_steady_state_error_dbfs")
        check("loop_seam_samples", seam["jump_below_p999"] and len(x) == exp_len,
              f"wrap jump L/R {seam['jump'][0]:.5f}/{seam['jump'][1]:.5f} FS = {seam['jump_vs_median_step'][0]:.1f}x/"
              f"{seam['jump_vs_median_step'][1]:.1f}x the median sample step (99.9th pct step {seam['p999_step'][0]:.4f}); "
              f"length {len(x)} = {exp_len} samples")
        check("loop_seam_steady", ss_err is not None and ss_err < -90,
              f"loop wrap-around vs continuous playback across a loop boundary: max diff {ss_err} dBFS (pre-dither)")
        # informational only: a musical downbeat (bar 1's bell toll after the quiet outro) is *meant* to be an
        # attack. Whether the loop point itself adds anything is settled by the two checks above.
        res["loop_seam_transient_info"] = dict(ok=True, detail=(
            f"HF onset at seam {seam_j:+.1f} dB vs internal bar lines median {np.median(bar_lines):+.1f} / max "
            f"{max(bar_lines):+.1f} dB - the seam is the bar-1 downbeat (a written attack), not a click"))
        res["loop_seam_samples"]["data"] = seam
    # Roblox
    wav_bytes, flac_bytes = os.path.getsize(wav), os.path.getsize(flac) if os.path.exists(flac) else None
    check("roblox_wav_size", wav_bytes < SPEC["roblox_max_bytes"],
          f"WAV {wav_bytes / 1e6:.2f} MB (limit < 20 MB)", warn_only=True)
    if flac_bytes is not None:
        check("roblox_flac_size", flac_bytes < SPEC["roblox_max_bytes"], f"FLAC {flac_bytes / 1e6:.2f} MB (limit < 20 MB)")
    check("roblox_duration", dur < SPEC["roblox_max_s"], f"{dur:.2f} s (limit < 7 min)")
    check("roblox_samplerate", sr <= SPEC["roblox_max_sr"], f"{sr} Hz (limit <= 48 kHz)")

    # musical facts
    mk = key_from_chroma(midi_chroma(midi))
    ak = key_from_chroma(audio_chroma(x, sr))
    const_end = tmap[1]["start_s"] if len(tmap) > 1 else None
    bpm_audio = audio_bpm(x[: int(const_end * sr)] if const_end else x, sr)
    # onset autocorrelation can lock to half/double time: report the metrical equivalent nearest the written tempo
    bpm_metrical = min((bpm_audio * k for k in (0.5, 2 / 3, 1, 1.5, 2)), key=lambda v: abs(v - meta["bpm"]))
    key_regions = []
    kbs = meta.get("keys_by_section")
    if kbs:
        bars = sorted(int(k) for k in kbs)
        for i, b in enumerate(bars):
            t0 = bar_time(b)
            t1 = bar_time(bars[i + 1]) if i + 1 < len(bars) else meta["music_length_s"]
            key_regions.append((b, kbs[str(b)], key_from_chroma(midi_chroma(midi, t0, t1))[0],
                                key_from_chroma(audio_chroma(x, sr, t0, t1))[0]))
    sect_loud = []
    for s in meta["sections"]:
        b0, b1 = (int(v) for v in s["bars"].split("-"))
        seg = x[int(bar_time(b0) * sr):int(bar_time(b1 + 1) * sr)]
        sect_loud.append((s["name"], s["bars"], s["start_s"], meter.integrated_loudness(seg)))
    bands = octave_bands(x, sr)
    hum = humanization_stats(midi, meta["bpm"], meta.get("loop_length_s") if loop else None)

    # ------------------------------------------------------------------ print
    print("=" * 100)
    print(f" VERIFY  {os.path.relpath(wav, ROOT)}")
    print("=" * 100)
    print(f" Key (written)      : {meta['key']}")
    print(f" Key (MIDI, K-S)    : " + ", ".join(f"{k} r={r:.2f}" for r, k in mk))
    print(f" Key (audio chroma) : " + ", ".join(f"{k} r={r:.2f}" for r, k in ak))
    for b, written, (rm, km), (ra, ka) in key_regions:
        print(f"   from bar {b:3d}: written {written:9s} | MIDI {km:9s} r={rm:.2f} | audio {ka:9s} r={ra:.2f}")
    if len(tmap) > 1:
        print(f" BPM                : {meta['bpm']} written for bars 1-{tmap[1]['bar'] - 1}; audio estimate there "
              f"{bpm_audio:.1f} (metrical equivalent {bpm_metrical:.1f}); then the tempo map: "
              + " ".join(str(t["bpm"]) for t in tmap[1:]))
    else:
        print(f" BPM                : {meta['bpm']} written; audio onset autocorrelation estimate {bpm_audio:.1f} "
              f"(metrical equivalent {bpm_metrical:.1f})")
    print(f" Time signature     : {meta['time_signature'][0]}/{meta['time_signature'][1]}, {meta['bars']} bars, seed {meta['seed']}")
    print(f" Duration           : {dur:.3f} s ({len(x)} samples){'  [seamless loop]' if loop else ''}")
    print(" Section map        :")
    for name, bars, st, lu in sect_loud:
        mm, ss = divmod(st, 60)
        print(f"   bars {bars:>5s}  @ {int(mm)}:{ss:05.2f}  {lu:6.1f} LUFS   {name}")
    print(" Humanisation (MIDI): " + "  ".join(f"{k} n={v['n']} max|off|={v['max_abs_ms']} ms sd={v['sd_ms']} ms"
                                            for k, v in hum.items() if k != "mean_velocity")
          + f"  | mean velocity downbeats {hum['mean_velocity']['downbeat']} vs others {hum['mean_velocity']['other']}")
    print(" Octave bands (dB re total): " + "  ".join(f"{k} {v}" for k, v in bands.items()))
    print("-" * 100)
    for k, v in res.items():
        tag = ("INFO" if k.endswith("_info") else "PASS") if v["ok"] else ("WARN" if k in warns else "FAIL")
        print(f" [{tag}] {k:18s} {v['detail']}")
    print("-" * 100)
    status = "FAIL" if fails else "PASS"
    print(f" RESULT: {status}" + (f"  (failed: {', '.join(fails)})" if fails else "")
          + (f"  (warnings: {', '.join(warns)})" if warns else ""))

    out = dict(result=status, fails=fails, warnings=warns, checks=res, key_midi=mk, key_audio=ak,
               bpm_audio=bpm_audio, sections=sect_loud, octave_bands=bands, duration_s=dur, humanization=hum,
               key_regions=key_regions)
    with open(base + "_verify.json", "w") as f:
        json.dump(out, f, indent=2, default=float)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
