"""compose.py - composition engine + CLI. Each piece lives in pieces/<name>.py.

    python compose.py --piece capital_city                      # -> output/capital_city/capital_city.mid (+ .json)
    python compose.py --piece temporal_shift --version battle   # -> output/temporal_shift_battle/...
    python compose.py --piece temporal_shift --version explore --seed 12

The engine supplies: chords/harmony, a tempo map (constant or stepped), voice-leading searches
(pad voicing, counter-line, bassline), ornaments, keyswitch articulations, long-note re-articulation,
humanisation, MIDI export and self-checks. Pieces supply the themes, form and arrangement.
Same piece + version + seed -> identical MIDI.
"""
import argparse
import importlib
import itertools
import json
import math
import os
from dataclasses import dataclass, field

import numpy as np
import pretty_midi

ROOT = os.path.dirname(os.path.abspath(__file__))

NOTE_PC = dict(C=0, D=2, E=4, F=5, G=7, A=9, B=11)
QUALITIES = {"": (0, 4, 7), "m": (0, 3, 7), "7": (0, 4, 7, 10), "sus4": (0, 5, 7), "m7": (0, 3, 7, 10),
             "maj7": (0, 4, 7, 11), "dim": (0, 3, 6), "m7b5": (0, 3, 6, 10), "aug": (0, 4, 8),
             "madd9": (0, 3, 7, 2), "add9": (0, 4, 7, 2)}
DYN = dict(pp=34, p=46, mp=60, mf=74, f=90, ff=104)
PC_NAMES = ["C", "Db", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"]


def P(name):
    """'C#5' -> 73 (C4 = 60)."""
    if isinstance(name, (int, np.integer)):
        return int(name)
    i, acc = 1, 0
    while i < len(name) and name[i] in "#b":
        acc += 1 if name[i] == "#" else -1
        i += 1
    return 12 * (int(name[i:]) + 1) + NOTE_PC[name[0]] + acc


def _pc(s):
    v = NOTE_PC[s[0]]
    for ch in s[1:]:
        v += 1 if ch == "#" else -1 if ch == "b" else 0
    return v % 12


@dataclass
class Chord:
    sym: str
    root: int
    tones: tuple
    bass: int
    third: object
    fifth: int


def parse_chord(sym):
    main, _, bass = sym.partition("/")
    i = 1
    while i < len(main) and main[i] in "#b":
        i += 1
    root = _pc(main[:i])
    q = main[i:]
    iv = QUALITIES[q]
    tones = tuple((root + t) % 12 for t in iv)
    third = None if q == "sus4" else (root + iv[1]) % 12
    return Chord(sym, root, tones, _pc(bass) if bass else root, third, (root + iv[2]) % 12)


def nearest_pc(pc, target, lo, hi):
    cands = [p for p in range(lo, hi + 1) if p % 12 == pc]
    return min(cands, key=lambda p: abs(p - target))


# =============================================================================================
# PIECE CONTEXT: form, harmony, tempo map, instruments
# =============================================================================================
class Piece:
    def __init__(self, title, brief, sections, progression, instruments, humanize, legato_parts,
                 scale_fn, tempo_map=None, melody_by_section=None, check_parts=(), pad_parts=("violins", "violas"),
                 bass_part="basses", keys_by_section=None):
        self.title = title
        self.brief = brief
        self.sections = sections
        self.progression = progression
        self.instruments = instruments
        self.order = list(instruments)
        self.humanize = humanize
        self.legato_parts = legato_parts
        self.scale_fn = scale_fn
        self.melody_by_section = melody_by_section or {1: None}
        self.check_parts = check_parts
        self.pad_parts = pad_parts
        self.bass_part = bass_part
        self.keys_by_section = keys_by_section or {1: brief["key_name"]}
        self.total_bars = len(progression)
        self.total_beats = self.total_bars * 4
        self.spans = self._spans()
        # tempo map: [(bar, bpm), ...] - bpm holds from that bar's downbeat until the next entry
        self.tempo_map = tempo_map or [(1, brief["bpm"])]
        self._segs = []
        sec = 0.0
        for i, (bar, bpm) in enumerate(self.tempo_map):
            b0 = (bar - 1) * 4
            if i:
                pb0, ps0, pspb = self._segs[-1]
                sec = ps0 + (b0 - pb0) * pspb
            self._segs.append((b0, sec, 60.0 / bpm))

    def _spans(self):
        out = []
        for bi, cell in enumerate(self.progression):
            parts = cell.split("|")
            for k, s in enumerate(parts):
                st = bi * 4 + k * 4 / len(parts)
                out.append((st, st + 4 / len(parts), parse_chord(s), bi + 1))
        return out

    # tempo ---------------------------------------------------------------------------------
    def _seg(self, beat):
        seg = self._segs[0]
        for s in self._segs:
            if s[0] <= beat + 1e-9:
                seg = s
        return seg

    def sec(self, beat):
        b0, s0, spb = self._seg(beat)
        return s0 + (beat - b0) * spb

    def spb_at(self, beat):
        return self._seg(beat)[2]

    def bpm_at_bar(self, bar):
        return 60.0 / self.spb_at((bar - 1) * 4)

    # harmony -------------------------------------------------------------------------------
    def chord_at(self, beat):
        for s, e, c, _ in self.spans:
            if s <= beat < e:
                return c
        return self.spans[-1][2]

    def spans_in(self, b0, b1):
        return [sp for sp in self.spans if b0 <= sp[3] <= b1]

    def scale_for(self, ch, beat=0.0):
        return self.scale_fn(ch, beat)

    def scale_step(self, p, direction, ch, beat=0.0):
        sc = self.scale_for(ch, beat)
        q = p + direction
        while q % 12 not in sc:
            q += direction
        return q

    def lead_part(self, bar):
        return self.melody_by_section[max(k for k in self.melody_by_section if k <= bar)]

    def section_of(self, bar):
        return max((s for s in self.sections if s["start"] <= bar), key=lambda s: s["start"])


# =============================================================================================
# SCORE
# =============================================================================================
@dataclass
class Note:
    start: float  # beats from the top
    dur: float    # beats
    pitch: int
    vel: int
    voice: int = 0
    grace: bool = False
    ks: bool = False   # keyswitch (articulation change), not a sounding note


class Score:
    def __init__(self, pc):
        self.pc = pc
        self.notes = {n: [] for n in pc.order}
        self.ccs = {n: [] for n in pc.order}
        self.bends = {n: [] for n in pc.order}

    def add(self, part, start, dur, pitch, vel, voice=0, grace=False):
        self.notes[part].append(Note(float(start), float(dur), int(P(pitch)), int(vel), voice, grace))

    def art(self, part, beat, art):
        """Switch `part` to articulation `art` from `beat` on (keyswitch note, sent just before the beat)."""
        spec = self.pc.instruments[part]
        if "ks_map" in spec:      # the instrument's own keyswitch notes (e.g. guitar 36/37/38)
            k = spec["ks_map"][art]
        else:
            k = 120 + [a[0] for a in spec["arts"]].index(art)
        self.notes[part].append(Note(float(beat), 0.05, k, 100, ks=True))

    def put(self, part, bar, items, vel, voice=0):
        """items: (beat_in_bar, dur, pitch-or-[pitches][, vel])"""
        base = (bar - 1) * 4
        for it in items:
            b, d, p = it[:3]
            v = it[3] if len(it) > 3 else vel
            for pp in (p if isinstance(p, (list, tuple)) else [p]):
                self.add(part, base + b, d, pp, v, voice)

    def put_many(self, part, bars_dict, vel, voice=0):
        for bar, items in bars_dict.items():
            self.put(part, bar, items, vel, voice)

    def roll(self, part, start, pitches, spread, dur, vel):
        ps = sorted(P(p) for p in pitches)
        for i, p in enumerate(ps):
            off = spread * i / max(1, len(ps) - 1)
            self.add(part, start + off, max(0.25, dur - off), p, vel - i)

    def notes_between(self, part, b0, b1):
        return sorted([n for n in self.notes[part] if b0 <= n.start < b1 and not n.ks], key=lambda n: n.start)


def sounding(S, part, t):
    return [n.pitch for n in S.notes[part] if n.start <= t + 1e-6 < n.start + n.dur and not n.grace and not n.ks]


# =============================================================================================
# GENERATORS
# =============================================================================================
def ornament(S, part, bar, beat, rng, kind="mordent", prob=1.0):
    """Replace the note at (bar, beat) by an ornamented figure (main-upper-main / turn)."""
    if rng.random() > prob:
        return
    t = (bar - 1) * 4 + beat
    for n in S.notes[part]:
        if abs(n.start - t) < 1e-6 and n.dur >= 1.0 and not n.grace and not n.ks:
            ch = S.pc.chord_at(t)
            up = S.pc.scale_step(n.pitch, +1, ch, t)
            if kind == "mordent":
                figure = [(n.pitch, 0.125), (up, 0.125)]
            else:  # turn: upper, main, lower, main
                lo = S.pc.scale_step(n.pitch, -1, ch, t)
                figure = [(up, 0.125), (n.pitch, 0.125), (lo, 0.125)]
            lo_r, hi_r = S.pc.instruments[part]["range"]
            if any(not (lo_r <= p <= hi_r) for p, _ in figure):
                return
            off = 0.0
            for p, d in figure:
                S.notes[part].append(Note(n.start + off, d, p, max(1, n.vel - 6), n.voice, grace=True))
                off += d
            n.start += off
            n.dur -= off
            return


def counter_line(S, part, b0, b1, mel_notes, lo, hi, center, vel, rng, mode="normal"):
    """Dynamic-programming counter-melody: chord tones on strong beats chosen for smooth motion,
    contrary motion to the melody and no parallel perfect intervals; passing/neighbour tones
    fill in wherever the melody is static."""
    pc = S.pc
    anchors = []
    for s, e, ch, bar in pc.spans_in(b0, b1):
        step = 2 if mode == "normal" else (e - s)
        t = s
        while t < e - 1e-6:
            anchors.append((t, min(step, e - t), ch))
            t += step

    def mel_at(t):
        cur = None
        for n in mel_notes:
            if n.start <= t + 1e-6:
                cur = n.pitch
        return cur

    def onsets(t, d):
        return sum(1 for n in mel_notes if t - 1e-6 <= n.start < t + d - 1e-6 and not n.grace)

    cands = []
    for t, d, ch in anchors:
        pcs = set(ch.tones)
        c = [p for p in range(lo, hi + 1) if p % 12 in pcs]
        cands.append(c)
    INF = 1e9
    cost = [{p: 0.08 * abs(p - center) + rng.random() * 0.01 for p in cands[0]}]
    back = [{}]
    for i in range(1, len(anchors)):
        t, d, ch = anchors[i]
        m_prev, m_cur = mel_at(anchors[i - 1][0]), mel_at(t)
        row, brow = {}, {}
        for q in cands[i]:
            local = 0.08 * abs(q - center)
            if m_cur is not None:
                if m_cur - q < 3:
                    local += 10
                if (m_cur - q) % 12 == 0:
                    local += 1.5
            if q % 12 == ch.bass:
                local += 0.4
            best, arg = INF, None
            for p, c0 in cost[i - 1].items():
                dlt = abs(q - p)
                tc = 0.3 * dlt if dlt <= 4 else 1.2 + 0.5 * (dlt - 4)
                if dlt == 0:
                    tc += 1.4
                if m_prev is not None and m_cur is not None and m_prev != m_cur and dlt:
                    if np.sign(q - p) == np.sign(m_cur - m_prev):
                        tc += 1.0
                    else:
                        tc -= 0.3
                    if (m_prev - p) % 12 in (0, 7) and (m_cur - q) % 12 == (m_prev - p) % 12:
                        tc += 6
                v = c0 + tc
                if v < best:
                    best, arg = v, p
            row[q] = best + local + rng.random() * 0.01
            brow[q] = arg
        cost.append(row)
        back.append(brow)
    p = min(cost[-1], key=cost[-1].get)
    path = [p]
    for i in range(len(anchors) - 1, 0, -1):
        p = back[i][p]
        path.append(p)
    path.reverse()

    for i, (t, d, ch) in enumerate(anchors):
        p = path[i]
        nxt = path[i + 1] if i + 1 < len(anchors) else None
        if mode == "normal" and d == 2 and nxt is not None and onsets(t, d) <= 1:
            if nxt != p:
                dirn = 1 if nxt > p else -1
                q = pc.scale_step(p, dirn, ch, t)
                if (q - nxt) * dirn >= 0:          # step would reach/overshoot: use a neighbour instead
                    q = pc.scale_step(p, -dirn, ch, t)
            else:
                q = pc.scale_step(p, +1, ch, t)
            if lo <= q <= hi:
                S.add(part, t, 1, p, vel)
                S.add(part, t + 1, 1, q, vel - 4)
                continue
        S.add(part, t, d, p, vel)
    merge_ties(S, part, b0, b1)


def merge_ties(S, part, b0, b1):
    t0, t1 = (b0 - 1) * 4, b1 * 4
    inside = sorted([n for n in S.notes[part] if t0 <= n.start < t1 and not n.ks], key=lambda n: (n.voice, n.start))
    others = [n for n in S.notes[part] if not (t0 <= n.start < t1) or n.ks]
    merged = []
    for n in inside:
        if (merged and merged[-1].voice == n.voice and merged[-1].pitch == n.pitch
                and abs(merged[-1].start + merged[-1].dur - n.start) < 1e-6 and not n.grace):
            merged[-1].dur += n.dur
        else:
            merged.append(n)
    S.notes[part] = others + merged


def voice_pad(S, b0, b1, voices, center, vel, rng, bass_part=None):
    """Viterbi voice-leading over chord voicings. voices = [(part, lo, hi), ...] bottom -> top.
    Penalises voice movement, parallel 5ths/8ves between voices and against the (already written) bass."""
    pc = S.pc
    bass_part = bass_part or pc.bass_part
    spans = pc.spans_in(b0, b1)
    n_v = len(voices)
    bass = [min(sounding(S, bass_part, s), default=None) for s, _, _, _ in spans]
    all_c = []
    for s, e, ch, _ in spans:
        pcs = set(ch.tones)
        pool = [p for p in range(min(v[1] for v in voices), max(v[2] for v in voices) + 1) if p % 12 in pcs]
        cs = []
        for combo in itertools.combinations(pool, n_v):
            if any(not (voices[k][1] <= combo[k] <= voices[k][2]) for k in range(n_v)):
                continue
            gaps = np.diff(combo)
            if np.any(gaps < 3) or (n_v > 1 and np.any(gaps[1:] > 12)) or (n_v > 1 and gaps[0] > 16):
                continue
            pcl = [p % 12 for p in combo]
            if ch.root not in pcl:
                continue
            if ch.third is not None and n_v >= 2 and ch.third not in pcl:
                continue
            if ch.third is not None and pcl.count(ch.third) > 1:
                continue
            if max(pcl.count(x) for x in set(pcl)) > 2:
                continue
            cs.append(combo)
        if not cs:
            raise RuntimeError(f"no voicing for {ch.sym} in bars {b0}-{b1}")
        all_c.append(np.array(cs))
    cost = 0.15 * np.abs(all_c[0].mean(1) - center) + rng.random(len(all_c[0])) * 0.01
    backs = []
    for i in range(1, len(all_c)):
        a, b = all_c[i - 1], all_c[i]
        mv = np.abs(a[:, None, :] - b[None, :, :]).sum(-1).astype(float)
        par = np.zeros_like(mv)
        for x, y in itertools.combinations(range(n_v), 2):
            ia = (a[:, y] - a[:, x]) % 12
            ib = (b[:, y] - b[:, x]) % 12
            same = (ia[:, None] == ib[None, :]) & np.isin(ia, (0, 7))[:, None]
            dx = np.sign(b[None, :, x] - a[:, None, x])
            dy = np.sign(b[None, :, y] - a[:, None, y])
            par += (same & (dx == dy) & (dx != 0)) * 8.0
        ba, bb = bass[i - 1], bass[i]
        if ba is not None and bb is not None and ba != bb:
            for x in range(n_v):
                ia = (a[:, x] - ba) % 12
                ib = (b[:, x] - bb) % 12
                same = (ia[:, None] == ib[None, :]) & np.isin(ia, (0, 7))[:, None]
                dx = np.sign(b[None, :, x] - a[:, None, x])
                par += (same & (dx == np.sign(bb - ba))) * 8.0
        tot = cost[:, None] + mv + par
        backs.append(tot.argmin(0))
        cost = tot.min(0) + 0.15 * np.abs(b.mean(1) - center) + rng.random(len(b)) * 0.01
    idx = [int(cost.argmin())]
    for bk in reversed(backs):
        idx.append(int(bk[idx[-1]]))
    idx.reverse()
    chosen = [all_c[i][j] for i, j in enumerate(idx)]
    for k, (part, _, _) in enumerate(voices):
        for (s, e, ch, _), combo in zip(spans, chosen):
            S.add(part, s, e - s, int(combo[k]), vel, voice=k)
    for part in {v[0] for v in voices}:
        merge_ties(S, part, b0, b1)
    return chosen


def harp_bars(S, b0, b1, pattern, span, vel, rng, prev_bass=43, part="harp", lo=38, hi=50, step=0.5):
    for s, e, ch, bar in S.pc.spans_in(b0, b1):
        bass = nearest_pc(ch.bass, prev_bass, lo, hi)
        prev_bass = bass
        pool = [p for p in range(bass, bass + span + 1) if p % 12 in ch.tones]
        per_bar = int(round(4 / step))
        k0 = int(round((s % 4) / step))
        for k in range(int(round((e - s) / step))):
            idx = pattern[(k0 + k) % len(pattern)]
            p = pool[min(idx, len(pool) - 1)]
            v = vel + (6 if (k0 + k) % per_bar == 0 else 0) - (3 if (k0 + k) % 2 else 0)
            S.add(part, s + step * k, 3 * step if (k0 + k) % 2 == 0 else 2 * step, p, v)
    return prev_bass


def harp_run(S, start, pitches, beats, vel0, vel1, part="harp"):
    n = len(pitches)
    for i, p in enumerate(pitches):
        S.add(part, start + beats * i / n, 1.0, p, int(vel0 + (vel1 - vel0) * i / max(1, n - 1)))


def bass_line(S, b0, b1, style, vel, rng, prev=38, mel_fn=None, part=None, ranges=None):
    """Bassline for bars b0..b1. Octave placement of every chord's bass note is chosen by a Viterbi
    search (smooth motion, register centre, and no parallel 5ths/8ves against the melody), then each
    bar is embellished according to `style`."""
    pc = S.pc
    part = part or pc.bass_part
    rng_lo, rng_hi, center = (ranges or {"half": (31, 50, 40), "ominous": (26, 45, 33), "stately": (31, 50, 40),
                                         "whole": (31, 48, 38), "eighths": (31, 50, 40)})[style]
    spans = pc.spans_in(b0, b1)
    nxt_span = next((sp for sp in pc.spans if sp[0] >= spans[-1][1] - 1e-6), None)
    seq = spans + ([nxt_span] if nxt_span else [])     # include the following chord for the last approach
    mel = [mel_fn(sp[0]) if mel_fn else None for sp in seq]

    def parallel(pb, pm, q, m):
        if None in (pb, pm, q, m) or m == pm or q == pb:
            return False
        return ((pm - pb) % 12 in (0, 7) and (m - q) % 12 == (pm - pb) % 12
                and np.sign(m - pm) == np.sign(q - pb))

    cands = [[x for x in range(rng_lo, rng_hi + 1) if x % 12 == sp[2].bass] for sp in seq]
    cost = {q: abs(q - prev) + 0.3 * abs(q - center) for q in cands[0]}
    backs = []
    for i in range(1, len(seq)):
        row, brow = {}, {}
        for q in cands[i]:
            best, arg = 1e9, None
            for p_, c0 in cost.items():
                c = c0 + abs(q - p_) + (30 if parallel(p_, mel[i - 1], q, mel[i]) else 0)
                if c < best:
                    best, arg = c, p_
            row[q] = best + 0.3 * abs(q - center)
            brow[q] = arg
        cost = row
        backs.append(brow)
    q = min(cost, key=cost.get)
    path = [q]
    for brow in reversed(backs):
        q = brow[q]
        path.append(q)
    path.reverse()
    pitch_at = {round(sp[0], 4): path[i] for i, sp in enumerate(seq)}

    for bar in range(b0, b1 + 1):
        sp = [x for x in pc.spans if x[3] == bar]
        t0 = (bar - 1) * 4
        if len(sp) == 2 and style != "eighths":
            for s_, e_, ch, _ in sp:
                S.add(part, s_, e_ - s_, pitch_at[round(s_, 4)], vel)
            prev = pitch_at[round(sp[-1][0], 4)]
            continue
        if style == "eighths":   # driving repeated 8ths with an octave kick on each chord
            for s_, e_, ch, _ in sp:
                p = pitch_at[round(s_, 4)]
                n8 = int(round((e_ - s_) / 0.5))
                for k in range(n8):
                    q8 = p + 12 if (k % 4 == 3 and p + 12 <= rng_hi) else p
                    S.add(part, s_ + 0.5 * k, 0.5, q8, vel + (6 if k % 2 == 0 else -4))
                prev = p
            continue
        nxt_t = round(sp[0][1], 4)
        target = pitch_at.get(nxt_t, prev)
        ch = sp[0][2]
        p = pitch_at[round(t0, 4)]
        prev = p
        # chord fifth (not the bass note's fifth: matters for inversions like C/E)
        fifth = min((x for x in range(rng_lo, rng_hi + 1) if x % 12 == ch.fifth and x != p),
                    key=lambda x: (abs(x - p) > 9, x < p, abs(x - p)))
        if style == "whole":
            S.add(part, t0, 4, p, vel)
        elif style == "ominous":
            up = p + 12 if p + 12 <= rng_hi else fifth
            S.add(part, t0, 3, p, vel)
            S.add(part, t0 + 3, 1, up, vel - 8)
        elif style == "half":
            if abs(target - p) <= 2 or rng.random() < 0.25:
                S.add(part, t0, 2, p, vel)
                S.add(part, t0 + 2, 2, p if rng.random() < 0.5 else fifth, vel - 6)
            else:
                dirn = 1 if target > p else -1
                appr = pc.scale_step(target, -dirn, ch, t0)
                S.add(part, t0, 2, p, vel)
                S.add(part, t0 + 2, 1, fifth if abs(fifth - appr) <= 7 else p, vel - 6)
                S.add(part, t0 + 3, 1, appr if rng_lo <= appr <= rng_hi else p, vel - 4)
        elif style == "twobeat":   # root on beat 1, chord fifth on beat 3 (pizzicato / waltz-like support)
            S.add(part, t0, 1, p, vel)
            S.add(part, t0 + 2, 1, fifth, vel - 8)
        elif style == "stately":
            octv = p + 12 if p + 12 <= rng_hi else p
            S.add(part, t0, 2, p, vel)
            S.add(part, t0 + 2, 1, fifth, vel - 6)
            S.add(part, t0 + 3, 1, octv, vel - 4)
    return prev


def loop_safe_articulations(S):
    """The articulation in force at the end of a loop must be the one bar 1 needs (early-played downbeats wrap
    to the end of the file): add a switch back just before the loop point where needed."""
    for part, spec in S.pc.instruments.items():
        if "arts" not in spec and "ks_map" not in spec:
            continue
        default = list(spec["ks_map"].values())[0] if "ks_map" in spec else 120
        ks = sorted([n for n in S.notes[part] if n.ks], key=lambda n: n.start)
        if not ks:
            continue
        first = ks[0].pitch if ks[0].start <= 1e-6 else default
        if ks[-1].pitch != first:
            S.notes[part].append(Note(float(S.pc.total_beats - 0.25), 0.05, first, 100, ks=True))


def timp_roll(S, start, beats, pitch, v0, v1, part="timpani", step=0.125):
    n = int(round(beats / step))
    for i in range(n):
        S.add(part, start + i * step, step, pitch, int(v0 + (v1 - v0) * i / max(1, n - 1)))


def cc_curve(S, part, cc, fn, b_from=0.0, b_to=None, step=None, wobble=0.0, rng=None):
    """Write a CC curve: fn(bar_position) -> value, sampled every `step` beats, with optional random walk."""
    step = step or S.pc.humanize["cc_step_beats"]
    b_to = S.pc.total_beats if b_to is None else b_to
    times = np.arange(b_from, b_to + 1e-9, step)
    walk = np.clip(np.cumsum(rng.normal(0, 0.6, len(times))), -wobble, wobble) if (rng is not None and wobble) \
        else np.zeros(len(times))
    last = None
    for i, t in enumerate(times):
        v = int(np.clip(round(fn(1 + t / 4) + walk[i]), 0, 127))
        if v != last:
            S.ccs[part].append((float(t), cc, v))
            last = v


def interp(points, bar_pos):
    return float(np.interp(bar_pos, [p[0] for p in points], [p[1] for p in points]))


def pedal_ccs(S, part="piano", gap_beats=8.01):
    """Sustain pedal: lift at every new attack, re-press shortly after; release after the last note of a group."""
    onsets = sorted({round(n.start, 4) for n in S.notes[part] if not n.ks})
    groups = []
    for t in onsets:
        if groups and t - groups[-1][-1] <= gap_beats:
            groups[-1].append(t)
        else:
            groups.append([t])
    lift_beats = S.pc.humanize["pedal_lift_s"] / S.pc.spb_at(0)
    for g in groups:
        for t in g:
            S.ccs[part].append((t, 64, 0))
            S.ccs[part].append((t + lift_beats, 64, 127))
        end = max(n.start + n.dur for n in S.notes[part] if g[0] <= n.start <= g[-1] + 1e-6 and not n.ks)
        S.ccs[part].append((end, 64, 0))


# =============================================================================================
# POST-PROCESSING: long-note re-articulation, humanisation, export
# =============================================================================================
def split_long_notes(S):
    pc = S.pc
    for part, spec in pc.instruments.items():
        mx = spec.get("max_note_s")
        if not mx:
            continue
        out = []
        for n in S.notes[part]:
            mxb = mx / pc.spb_at(n.start)
            if n.ks or n.dur <= mxb:
                out.append(n)
                continue
            k = math.ceil(n.dur / mxb)
            seg = n.dur / k
            for i in range(k):
                out.append(Note(n.start + i * seg, seg, n.pitch, max(1, n.vel - (8 if i else 0)), n.voice))
        S.notes[part] = out


def metric_pos(t):
    r = t % 4
    if abs(r) < 1e-6 or abs(r - 4) < 1e-6:
        return "downbeat"
    if abs(r - 2) < 1e-6:
        return "beat3"
    if abs(r - round(r)) < 1e-6:
        return "beat"
    return "offbeat"


def humanize(S, rng):
    pc = S.pc
    H = pc.humanize
    loop = pc.brief["loop"]
    loop_len = pc.sec(pc.total_beats)
    perf = {}
    for part in pc.order:
        all_notes = sorted(S.notes[part], key=lambda n: (n.start, n.pitch))
        ks_notes = [n for n in all_notes if n.ks]
        notes = [n for n in all_notes if not n.ks]
        tight = 0.6 if part in H["tight_parts"] else 1.0
        nb = pc.total_beats + 16
        walk = np.convolve(np.cumsum(rng.normal(0, 1, nb)), np.ones(8) / 8, mode="same")
        walk = walk - walk.mean()
        walk = walk / (np.abs(walk).max() + 1e-9) * H["drift_ms"]
        groups = {}
        for n in notes:
            groups.setdefault(round(n.start, 4), []).append(n)
        starts = {}
        for t, grp in groups.items():
            lim = H["timing_ms"][metric_pos(t)] * tight
            base = rng.normal(0, lim / 2.5) + walk[min(int(t), nb - 1)]
            for n in grp:
                off = float(np.clip(base + rng.normal(0, H["chord_spread_ms"] / 2), -lim, lim)) / 1000.0
                st = pc.sec(t) + off
                if st < 0:
                    # a downbeat played early: in a loop it belongs just before the seam (the render's tail
                    # fold puts it back across the seam exactly as continuous playback would)
                    st = st + loop_len if loop else 0.0
                starts[id(n)] = st
        events = []
        legato = part in pc.legato_parts
        if part in H.get("exact_parts", ()):     # e.g. reversed swells: their end must land exactly on the beat
            perf[part] = sorted([(pc.sec(n.start), pc.sec(n.start + n.dur), n.pitch, n.vel) for n in notes]
                                + [(max(0.0, pc.sec(n.start) - 0.03), max(0.0, pc.sec(n.start) - 0.03) + 0.02,
                                    n.pitch, 100, True) for n in ks_notes])
            continue
        by_voice = {}
        for n in notes:
            by_voice.setdefault(n.voice, []).append(n)
        for v_notes in by_voice.values():
            v_notes.sort(key=lambda n: n.start)
            for i, n in enumerate(v_notes):
                s = starts[id(n)]
                t = n.start
                vel = n.vel
                if not n.grace:
                    pos = metric_pos(t)
                    vel += H["downbeat_accent"] if pos == "downbeat" else H["beat3_accent"] if pos == "beat3" else 0
                    vel += H["phrase_arch"] * (math.sin(math.pi * ((t % 16) / 16)) - 0.4)
                vel += rng.uniform(-H["velocity_jitter"], H["velocity_jitter"])
                vel = int(np.clip(round(vel), 1, 127))
                written_end = pc.sec(n.start + n.dur)
                if legato:
                    nxt = v_notes[i + 1] if i + 1 < len(v_notes) else None
                    if nxt is not None and abs(nxt.start - (n.start + n.dur)) < 0.02:
                        e = starts[id(nxt)] + rng.uniform(*H["legato_overlap_s"])
                    else:
                        e = written_end - n.dur * pc.spb_at(n.start) * rng.uniform(*H["release_trim"])
                else:
                    e = s + n.dur * pc.spb_at(n.start) * (1 + rng.uniform(-H["length_jitter"], H["length_jitter"]))
                e = max(e, s + 0.03)
                events.append((s, e, n.pitch, vel))
        # keyswitches: 30 ms before the beat (before any humanised early note), no jitter
        for n in ks_notes:
            s = max(0.0, pc.sec(n.start) - 0.03)
            events.append((s, s + 0.02, n.pitch, 100, True))
        perf[part] = sorted(events)
    return perf


def export(S, perf, out_path):
    pc = S.pc
    B = pc.brief
    transpose = ((B["tonic"] - B.get("written_tonic", B["tonic"]) + 6) % 12) - 6
    pm = pretty_midi.PrettyMIDI(initial_tempo=pc.tempo_map[0][1], resolution=960)
    if len(pc.tempo_map) > 1:   # embed the tempo map so DAWs show the accelerando
        scales = [(int(round(b0 * pm.resolution)), spb / pm.resolution) for b0, _, spb in pc._segs]
        pm._tick_scales = scales
        pm._update_tick_to_time(int(round((pc.total_beats + 64) * pm.resolution)))
    pm.time_signature_changes.append(pretty_midi.TimeSignature(*B["time_signature"], 0))
    for bar, key in sorted(pc.keys_by_section.items()):
        pm.key_signature_changes.append(pretty_midi.KeySignature(pretty_midi.key_name_to_key_number(key),
                                                                 pc.sec((bar - 1) * 4)))
    for name in pc.order:
        spec = pc.instruments[name]
        bank, prog = spec["gm"]
        inst = pretty_midi.Instrument(program=0 if bank == 128 else prog, is_drum=(bank == 128), name=name)
        lo, hi = spec["range"]
        shift = 0 if bank == 128 else transpose
        for ev in perf[name]:
            s, e, p, v = ev[:4]
            q = p
            if len(ev) == 4 and p < 120:      # keyswitches are never transposed
                q = p + shift
                while q > hi:
                    q -= 12
                while q < lo:
                    q += 12
            inst.notes.append(pretty_midi.Note(velocity=v, pitch=q, start=s, end=e))
        for t, c, val in sorted(S.ccs[name]):
            inst.control_changes.append(pretty_midi.ControlChange(number=c, value=int(val), time=pc.sec(t)))
        for t, val in sorted(S.bends.get(name, [])):
            inst.pitch_bends.append(pretty_midi.PitchBend(pitch=int(val), time=pc.sec(t)))
        pm.instruments.append(inst)
    if hasattr(pm, "lyrics"):
        for sec in pc.sections:
            pm.lyrics.append(pretty_midi.Lyric(sec["name"], pc.sec((sec["start"] - 1) * 4)))
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
    pm.write(out_path)


# =============================================================================================
# SELF-CHECKS (ranges, repetition, voice leading)
# =============================================================================================
def analyse(S):
    pc = S.pc
    report = {}
    bad = []
    for part, notes in S.notes.items():
        lo, hi = pc.instruments[part]["range"]
        for n in notes:
            if not n.ks and not lo <= n.pitch <= hi:
                bad.append((part, n.pitch, 1 + n.start / 4))
    report["out_of_range"] = bad

    def window(parts, bar):
        t0, t1 = (bar - 1) * 4, (bar + 3) * 4
        return tuple(sorted((p, round(n.start - t0, 3), round(n.dur, 3), n.pitch)
                            for p in parts for n in S.notes[p] if t0 <= n.start < t1 and not n.ks))
    dup_full = []
    wins = {}
    for bar in range(1, pc.total_bars - 2):
        w = window(pc.order, bar)
        if w in wins:
            dup_full.append((wins[w], bar))
        wins.setdefault(w, bar)
    report["identical_4bar_windows_full_arrangement"] = dup_full
    dup_parts = {}
    for part in pc.check_parts:
        seen, d = {}, []
        for bar in range(1, pc.total_bars - 2):
            w = window([part], bar)
            if len(w) < 4:
                continue
            if w in seen:
                d.append((seen[w], bar))
            seen.setdefault(w, bar)
        dup_parts[part] = d
    report["identical_4bar_windows_by_part"] = dup_parts

    # outer voices vs bass: parallel perfect 5ths / octaves between consecutive chord changes
    def parallels(top_fn):
        par, prev = [], None
        for s, e, ch, bar in pc.spans:
            tp = top_fn(s, bar)
            b = sounding(S, pc.bass_part, s)
            if tp is None or not b:
                prev = None
                continue
            mp, bp = tp, min(b)
            if prev and (mp - bp) % 12 in (0, 7) and (prev[0] - prev[1]) % 12 == (mp - bp) % 12 \
                    and mp != prev[0] and np.sign(mp - prev[0]) == np.sign(bp - prev[1]):
                par.append(bar)
            prev = (mp, bp)
        return par

    def lead_top(s, bar):
        part = pc.lead_part(bar)
        ps = sounding(S, part, s) if part else []
        return max(ps) if ps else None

    def pad_top(s, bar):
        ps = [p for part in pc.pad_parts for p in sounding(S, part, s)]
        return max(ps) if ps and pc.lead_part(bar) not in pc.pad_parts else None
    report["parallel_perfects_lead_vs_bass_bars"] = parallels(lead_top)
    report["parallel_perfects_pad_top_vs_bass_bars"] = parallels(pad_top)

    moves = []
    for part in pc.pad_parts:
        by_v = {}
        for n in S.notes[part]:
            if not n.ks:
                by_v.setdefault(n.voice, []).append(n)
        for vn in by_v.values():
            vn.sort(key=lambda n: n.start)
            for a, b in zip(vn, vn[1:]):
                if abs(b.start - (a.start + a.dur)) < 0.01:
                    moves.append(abs(b.pitch - a.pitch))
    report["pad_voice_motion_semitones"] = dict(mean=round(float(np.mean(moves)), 2), max=int(max(moves))) \
        if moves else {}
    return report


# =============================================================================================
def load_piece(name):
    return importlib.import_module(f"pieces.{name}")


def build(piece_name, version, seed, out_path=None):
    mod = load_piece(piece_name)
    S, extra = mod.compose(version, seed)
    pc = S.pc
    rep = analyse(S)
    split_long_notes(S)
    perf = humanize(S, np.random.default_rng(seed + 1000))
    out_path = out_path or os.path.join(ROOT, "output", pc.title, f"{pc.title}.mid")
    export(S, perf, out_path)
    loop_len = pc.sec(pc.total_beats)
    B = pc.brief
    meta = dict(
        title=pc.title, piece=piece_name, version=version, instrument_set=extra.get("instrument_set", piece_name),
        mood=B["mood"], seed=seed, bpm=pc.tempo_map[0][1],
        key=PC_NAMES[B["tonic"]] + " minor",
        time_signature=list(B["time_signature"]), bars=pc.total_bars, loop=B["loop"],
        loop_length_s=loop_len if B["loop"] else None, music_length_s=loop_len,
        sections=[dict(name=s["name"], bars=f"{s['start']}-{s['start'] + s['bars'] - 1}",
                       start_s=round(pc.sec((s["start"] - 1) * 4), 3)) for s in pc.sections],
        progression=pc.progression, note_counts={k: sum(1 for n in v if len(n) == 4) for k, v in perf.items()},
        checks=rep,
    )
    if len(pc.tempo_map) > 1:
        meta["tempo_map"] = [dict(bar=b, bpm=bpm, start_s=round(pc.sec((b - 1) * 4), 3)) for b, bpm in pc.tempo_map]
    if len(pc.keys_by_section) > 1:
        meta["keys_by_section"] = {str(k): v for k, v in sorted(pc.keys_by_section.items())}
    for k, v in extra.items():
        if k != "instrument_set":
            meta[k] = v
    with open(os.path.splitext(out_path)[0] + ".json", "w") as f:
        json.dump(meta, f, indent=2)
    return out_path, meta, rep


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--piece", default="capital_city")
    ap.add_argument("--version", default=None, help="arrangement, for pieces that have several (e.g. explore/battle)")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    out, meta, rep = build(a.piece, a.version, a.seed, a.out)
    print(f"wrote {out}  ({meta['bars']} bars, {meta['music_length_s']:.2f} s, seed {a.seed})")
    for k, v in rep.items():
        print(f"  {k}: {v}")
    if rep["out_of_range"]:
        raise SystemExit("notes out of instrument range")


if __name__ == "__main__":
    main()
