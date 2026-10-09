"""Shared composing toolkit for the build scripts in daw/projects (each one pushes a song into the live Flux session).

    sys.path.insert(0, os.path.join(ROOT, "daw", "bridge"))
    import score
    score.setup(bpb=3, unit=0.5, pause=0.05)          # 6/8: three quarter-note beats a bar, grid unit an eighth
    from score import at, n, curve, melody, push, reset, focus, edit

Session   call(tool, args)  edit(*cmds)  project()  focus(track, bar, panel)  push(track, notes, label)  reset(*tracks)
          sync_tracks(TRACKS)  apply_mix(MIX, MASTER, save=PATH)  run(STEPS, globals())  sfz(path, c70=108, ...)
Time      at(bar, x=0.0) = absolute beat of a 1-based bar plus x grid units;  bars_in((9, 16), (73, 80)) = bar numbers
Notes/CC  n(beat, dur, pitch, vel)  cc(beat, value)  ramp(b0, v0, b1, v1, step)  curve((bar, x, value), ...)
Notation  P("F#4") = 66, note_name(66) = "F#4";  melody("D5:3 G5:3 | F5:2 E5:1 D5:2 C5:1", bar, vel): pitch:units,
          "r" = rest, "|" = barline (each bar fills bpb/unit units);  tokens(text);  bars_of(text, first, count)
Harmony   chord("F/G") -> Chord;  voice_lead(["G", "Em", "C", "Dsus"], voices=4, low=55, high=79) -> .pitches .warnings
          bass_line(chords, low=36, high=55) -> pitches
Phrasing  phrase(notes, "6/8") -> the line with metric accents, an arc, soft endings, a slight rit. and breaths
          swell_cc(notes, lo, hi) -> CC1 points;  metric_weight(beat, meter)
Harmony and Phrasing are opt-in: nothing else here calls them.
"""
import itertools
import json
import random
import re
import sys
import time
from collections import namedtuple

import live

BPB, UNIT, PAUSE = 4, 1, 0.1      # quarter-note beats per bar, beats per grid unit, seconds after each visible step


def setup(bpb=4, unit=1, pause=0.1):
    """the song's grid: `bpb` quarter-note beats per bar (6/8 = 3), `unit` beats per grid step for at, curve and the
    notation (an eighth = 0.5), `pause` seconds after each visible step so a human can follow in the window"""
    global BPB, UNIT, PAUSE
    BPB, UNIT, PAUSE = bpb, unit, pause


# ---------------------------------------------------------------------------------------------
# session

def call(tool, args=None):
    r = live.send(live.PORT, {"tool": tool, "args": args or {}})
    text = "\n".join(r["text"])
    if r["isError"]:
        raise SystemExit(f"{tool} failed: {text}")
    return text


def edit(*cmds):
    return call("edit", {"commands": list(cmds)})


def project():
    return json.loads(call("get_project", {"detail": "full"}))["project"]


def focus(track=None, bar=None, panel=None):
    a = {"action": "focus"}
    if track:
        a["track"] = track
    if bar:
        a["bar"] = bar
    if panel:
        a["panel"] = panel
    call("gui", a)
    time.sleep(PAUSE)


def push(track, notes, label):
    """Add notes in chunks of a few hundred so each lands as one visible step."""
    notes = sorted(notes, key=lambda x: (x["beat"], x["pitch"]))
    for i in range(0, len(notes), 400):
        edit({"op": "add_notes", "track": track, "notes": notes[i:i + 400]})
    print(f"  {track}: {label} ({len(notes)} notes)")
    time.sleep(PAUSE)


def reset(*tracks):
    edit(*[c for t in tracks for c in ({"op": "remove_notes", "track": t}, {"op": "clear_cc", "track": t})])


def sfz(path, **ccs):
    d = {"kind": "sfz", "path": path}
    if ccs:
        d["init_cc"] = {k.lstrip("c"): v for k, v in ccs.items()}
    return d


def same_inst(have, want):
    """compare a stored instrument with a TRACKS entry (stored init_cc keys are numbers, ours are strings)"""
    norm = lambda d: {k: ({str(kk): vv for kk, vv in v.items()} if isinstance(v, dict) else v) for k, v in d.items() if v not in ({}, None)}
    return norm(have) == norm(want)


def sync_tracks(tracks):
    """make the session's tracks match [(name, instrument, color)]: add, re-instrument, remove"""
    have = {t["name"]: t for t in project()["tracks"]}
    for name, inst, color in tracks:
        if name not in have:
            edit({"op": "add_track", "name": name, "instrument": inst, "color": color})
            print(f"  + {name}")
            time.sleep(PAUSE * 0.4)
        elif not same_inst(have[name]["instrument"], inst):
            edit({"op": "update_track", "track": name, "instrument": inst})
            print(f"  ~ {name}: instrument updated")
    for name in have:
        if name not in {t[0] for t in tracks}:
            edit({"op": "remove_track", "track": name})
            print(f"  - {name}")


def apply_mix(mix, master, save=None):
    focus(panel="mixer")
    for name, m in mix.items():
        edit({"op": "set_mixer", "track": name, "mixer": m})
    edit({"op": "set_master", "master": master})
    if save:
        call("save", {"path": save})
    print("  mixer and master set" + (", saved" if save else ""))


def run(steps, scope):
    """entry point of a build script: the steps named on the command line (default: all), in order"""
    for s in sys.argv[1:] or steps:
        print(f"[{s}]")
        scope[f"step_{s}"]()


# ---------------------------------------------------------------------------------------------
# time, notes, CC

def at(bar, x=0.0):
    """absolute beat (0-based) of a 1-based bar plus `x` grid units"""
    return (bar - 1) * BPB + x * UNIT


def bars_in(*ranges):
    """bar numbers of inclusive (first, last) ranges"""
    return [b for a, z in ranges for b in range(a, z + 1)]


def n(beat, dur, pitch, vel):
    return {"beat": round(beat, 4), "dur": round(max(dur, 0.02), 4), "pitch": int(pitch), "vel": int(max(1, min(127, round(vel))))}


def cc(beat, value):
    return {"beat": round(max(beat, 0), 4), "value": int(max(0, min(127, round(value))))}


def ramp(b0, v0, b1, v1, step=0.5):
    """CC points every `step` beats from (b0, v0) to (b1, v1)"""
    k = max(1, int(round((b1 - b0) / step)))
    return [cc(b0 + (b1 - b0) * i / k, v0 + (v1 - v0) * i / k) for i in range(k + 1)]


def curve(*pts):
    """piecewise-linear CC curve through (bar, x, value) points"""
    out = []
    for (ba, ea, va), (bb, eb, vb) in zip(pts, pts[1:]):
        out += ramp(at(ba, ea), va, at(bb, eb), vb)[:-1]
    b, e, v = pts[-1]
    return out + [cc(at(b, e), v)]


# ---------------------------------------------------------------------------------------------
# notation: "D5:3 G5:3 | F5:2 E5:1 D5:2 C5:1" = pitch:units, "r" = rest, "|" = barline

PC = {"C": 0, "D": 2, "E": 4, "F": 5, "G": 7, "A": 9, "B": 11}
SHARPS = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]


def P(name):
    m = re.fullmatch(r"([A-G])([#b]?)(-?\d)", name)
    return 12 * (int(m[3]) + 1) + PC[m[1]] + {"#": 1, "b": -1, "": 0}[m[2]]


def note_name(p):
    return f"{SHARPS[p % 12]}{p // 12 - 1}"


def tokens(text):
    out = []
    for b in text.split("|"):
        total = 0
        for t in b.split():
            p, d = t.split(":")
            total += float(d)
            out.append((None if p == "r" else P(p), float(d)))
        assert abs(total - BPB / UNIT) < 1e-6, f"bar adds up to {total}: {b}"
    return out


def melody(text, bar, vel, shift=0, legato=1.0, short=0.85, arc=0):
    """Lay a phrase from `bar`. Notes run into the next note (legato) and are shortened before rests and at the end.
    `arc` adds up to that many velocity points towards the middle of each phrase (between rests)."""
    toks = tokens(text)
    t, out = at(bar), []
    for i, (p, d) in enumerate(toks):
        if p is not None:
            nxt = toks[i + 1][0] if i + 1 < len(toks) else None
            L = d * UNIT * (legato if nxt is not None else short)
            pos = i / max(1, len(toks) - 1)
            out.append(n(t, L, p + shift, vel + arc * (1 - abs(2 * pos - 1))))
        t += d * UNIT
    return out


def bars_of(text, first, count):
    """bars [first, first+count) of a notation string (1-based within the phrase)"""
    return "|".join(text.split("|")[first - 1:first - 1 + count])


# ---------------------------------------------------------------------------------------------
# harmony: chord symbols, voice leading, bass lines

QUALITY = {  # chord suffix -> intervals above the root
    "": (0, 4, 7), "m": (0, 3, 7), "dim": (0, 3, 6), "aug": (0, 4, 8), "5": (0, 7),
    "sus": (0, 5, 7), "sus2": (0, 2, 7), "6": (0, 4, 7, 9), "m6": (0, 3, 7, 9), "6/9": (0, 4, 7, 9, 2),
    "7": (0, 4, 7, 10), "maj7": (0, 4, 7, 11), "m7": (0, 3, 7, 10), "mmaj7": (0, 3, 7, 11), "m7b5": (0, 3, 6, 10),
    "dim7": (0, 3, 6, 9), "7sus": (0, 5, 7, 10), "9": (0, 4, 7, 10, 2), "maj9": (0, 4, 7, 11, 2), "m9": (0, 3, 7, 10, 2),
    "9sus": (0, 5, 7, 10, 2), "add9": (0, 4, 7, 2), "madd9": (0, 3, 7, 2), "11": (0, 7, 10, 2, 5),
    "m11": (0, 3, 7, 10, 2, 5), "13": (0, 4, 7, 10, 2, 9), "maj13": (0, 4, 7, 11, 2, 9), "m13": (0, 3, 7, 10, 2, 9),
}
ALIAS = {"maj": "", "M": "", "min": "m", "-": "m", "+": "aug", "°": "dim", "sus4": "sus", "M7": "maj7", "Δ": "maj7",
         "Δ7": "maj7", "-7": "m7", "min7": "m7", "ø": "m7b5", "ø7": "m7b5", "°7": "dim7", "69": "6/9", "m69": "m6/9",
         "7sus4": "7sus", "9sus4": "9sus", "add2": "add9", "M9": "maj9", "min9": "m9", "M13": "maj13"}
QUALITY["m6/9"] = (0, 3, 7, 9, 2)
PRIORITY = ("third", "sus", "seventh", "root", "ext", "fifth")          # what a thin voicing keeps first
DOUBLE = {"root": 0, "fifth": 0.5, "third": 1.5, "sus": 2, "ext": 4, "seventh": 6}   # cost of doubling a tone

Chord = namedtuple("Chord", "name root bass pcs roles third major")
VoiceLeading = namedtuple("VoiceLeading", "pitches warnings")


def _pc(s):
    return (PC[s[0]] + {"#": 1, "b": -1}.get(s[1:], 0)) % 12


def _roles(root, ivs):
    third = 4 if 4 in ivs else 3 if 3 in ivs else None
    fifth = 7 if 7 in ivs else 6 if third == 3 and 6 in ivs else 8 if third == 4 and 8 in ivs else None
    seventh = 10 if 10 in ivs else 11 if 11 in ivs else 9 if third == 3 and fifth == 6 and 9 in ivs else None
    sus = None if third else 5 if 5 in ivs else 2 if 2 in ivs else None     # sus4 (or sus2) stands in for the third
    roles = {}
    for i in ivs:
        r = ("root" if i == 0 else "third" if i == third else "fifth" if i == fifth else "seventh" if i == seventh
             else "sus" if i == sus else "ext")
        roles[(root + i) % 12] = r
    return roles


def chord(c):
    """a Chord from a symbol ("G", "Am7", "F/G", "Dsus", "Gmaj9", "A9sus", "G6/9", "C#m7b5/E"), a pitch-class set
    (the first element of a list/tuple is the root) or note names; `bass` is the slash bass (else the root)"""
    if isinstance(c, Chord):
        return c
    if isinstance(c, str):
        m = re.fullmatch(r"([A-G][#b]?)(.*?)(?:/([A-G][#b]?))?", c.strip())
        if not m or ALIAS.get(m[2], m[2]) not in QUALITY:
            raise ValueError(f"unknown chord symbol {c!r}")
        root = _pc(m[1])
        roles = _roles(root, QUALITY[ALIAS.get(m[2], m[2])])
        bass = _pc(m[3]) if m[3] else root
        name = c
    else:
        pcs = [x if isinstance(x, int) else _pc(x) for x in (c if isinstance(c, (list, tuple)) else sorted(c))]
        root = pcs[0] % 12
        roles = _roles(root, sorted({(p - root) % 12 for p in pcs}))
        bass, name = root, "{" + " ".join(SHARPS[p % 12] for p in pcs) + "}"
    third = next((p for p, r in roles.items() if r == "third"), None)
    major = third is not None and (third - root) % 12 == 4
    return Chord(name, root, bass, tuple(roles), roles, third, major)


def _need(c, voices):
    """the pitch classes every voicing of `c` with `voices` voices must contain"""
    tones = [p for p in c.pcs if not (c.major and p == c.third and c.bass == c.third != c.root)]
    tones.sort(key=lambda p: PRIORITY.index(c.roles[p]))
    return set(tones[:voices])


def _voicings(c, voices, low, high, cap=150):
    need = _need(c, voices)
    no_third = c.major and c.bass == c.third != c.root          # first inversion: the bass already has the third
    pool = [p for p in range(low, high + 1) if p % 12 in c.roles]
    mid = (low + high) / 2
    out = []
    for v in itertools.combinations(pool, voices):
        pcs = [p % 12 for p in v]
        if not need <= set(pcs) or (c.major and pcs.count(c.third) > (0 if no_third else 1)):
            continue
        cost = sum(DOUBLE[c.roles[p]] * (pcs.count(p) - 1) for p in set(pcs))
        for a, b in zip(v, v[1:]):
            cost += 3 + 0.3 * (b - a - 12) if b - a > 12 else 1.5 if b - a == 1 else 0
        out.append((cost + 0.08 * abs(sum(v) / voices - mid), v))
    if not out:
        raise ValueError(f"no {voices}-voice voicing of {c.name} fits in {note_name(low)}-{note_name(high)}")
    out.sort()
    return out[:cap]


def _parallel(a0, a1, b0, b1):
    """'fifths'/'octaves' if two voices (a: lower, b: upper) move in parallel perfect intervals"""
    i0, i1 = (b0 - a0) % 12, (b1 - a1) % 12
    if i0 == i1 and i0 in (0, 7) and a0 != a1 and (a1 - a0) * (b1 - b0) > 0:
        return "octaves" if i0 == 0 else "fifths"
    return None


def _move(a, b, pcs_b, bass=None):
    """cost of moving voicing a to b (`bass` = the (old, new) bass under them, if known)"""
    cost = 0
    for i, (x, y) in enumerate(zip(a, b)):
        d = abs(x - y)
        cost += d + max(0, d - 7)                                   # leaps past a fifth count double
        if d and x % 12 in pcs_b:
            cost += 1.5                                             # a common tone that could have stayed
        if i + 1 < len(a) and (y > a[i + 1] or b[i + 1] < x):
            cost += 3                                               # overlap: a voice passes its neighbour's old note
    top = len(a) - 1
    for i, j in itertools.combinations(range(len(a)), 2):
        if _parallel(a[i], b[i], a[j], b[j]):
            cost += 8 if (i, j) == (0, top) and not bass else 4
    for j in range(len(a)) if bass else ():
        if _parallel(bass[0], bass[1], a[j], b[j]):
            cost += 8 if j == top else 4
    return cost


def voice_lead(chords, voices=4, low=48, high=79, start=None, bass=None):
    """Voice a chord progression with the least total movement: voices stay in [low, high] and never cross, common
    tones are held, the third of a major chord is never doubled, thin voicings drop the fifth first and parallel
    fifths/octaves are avoided. `start` = the voicing to lead from; `bass` = the bass pitch under each chord (e.g. from
    bass_line): the voices then stay above it and avoid parallels with it.
    Returns VoiceLeading(pitches=[(low..high) per chord], warnings=[parallel fifths/octaves between outer voices])."""
    cs = [chord(c) for c in chords]
    cands, warnings = [], []
    for k, c in enumerate(cs):
        vs = _voicings(c, voices, low, high)
        if bass:
            above = [x for x in vs if x[1][0] > bass[k]]
            if not above:
                warnings.append(f"chord {k + 1} ({c.name}): no voicing above the bass {note_name(bass[k])}")
            vs = above or vs
        cands.append(vs)
    start = tuple(sorted(start)) if start else None
    back = [[(cost + (_move(start, v, cs[0].roles) if start else 0), None) for cost, v in cands[0]]]
    for k in range(1, len(cs)):
        prev, row, pair = back[-1], [], (bass[k - 1], bass[k]) if bass else None
        for cost, v in cands[k]:
            total, j = min((prev[i][0] + _move(u, v, cs[k].roles, pair), i) for i, (_, u) in enumerate(cands[k - 1]))
            row.append((cost + total, j))
        back.append(row)
    i = min(range(len(back[-1])), key=lambda i: back[-1][i][0])
    path = []
    for k in range(len(cs) - 1, -1, -1):
        path.append(cands[k][i][1])
        i = back[k][i][1]
    path.reverse()
    for k in range(1, len(path)):
        lo0, lo1 = (bass[k - 1], bass[k]) if bass else (path[k - 1][0], path[k][0])
        kind = _parallel(lo0, lo1, path[k - 1][-1], path[k][-1])
        if kind:
            warnings.append(f"chord {k}->{k + 1} ({cs[k - 1].name} -> {cs[k].name}): parallel {kind} between outer voices "
                            f"({note_name(lo0)}-{note_name(path[k - 1][-1])} -> {note_name(lo1)}-{note_name(path[k][-1])})")
    return VoiceLeading(path, warnings)


def bass_line(chords, low=36, high=55, start=None, inversions=True):
    """one bass pitch per chord: the slash bass of inversions (else the root), in the octave that moves least
    (leaps past a fifth cost double), leaning to the lower part of the range"""
    cs = [chord(c) for c in chords]
    home = low + (high - low) / 3
    cands = []
    for c in cs:
        pc = c.bass if inversions else c.root
        ps = [p for p in range(low, high + 1) if p % 12 == pc]
        if not ps:
            raise ValueError(f"no {SHARPS[pc]} for {c.name} in {note_name(low)}-{note_name(high)}")
        cands.append(ps)
    leap = lambda a, b: abs(a - b) + max(0, abs(a - b) - 7)
    row = [(leap(start, p) if start else 0) + 0.1 * abs(p - home) for p in cands[0]]
    back = []
    for k in range(1, len(cs)):
        nxt, ptr = [], []
        for p in cands[k]:
            j = min(range(len(row)), key=lambda i: row[i] + leap(cands[k - 1][i], p))
            nxt.append(row[j] + leap(cands[k - 1][j], p) + 0.1 * abs(p - home))
            ptr.append(j)
        row = nxt
        back.append(ptr)
    i = min(range(len(row)), key=lambda i: row[i])
    out = [cands[-1][i]]
    for k in range(len(cs) - 2, -1, -1):
        i = back[k][i]
        out.append(cands[k][i])
    return out[::-1]


# ---------------------------------------------------------------------------------------------
# phrasing: shape a line like a player would

def meter_of(meter):
    """(numerator, denominator) from "6/8" or (6, 8)"""
    num, den = map(int, meter.split("/")) if isinstance(meter, str) else meter
    return num, den


def metric_weight(beat, meter):
    """weight of an absolute beat (bar 1 at 0) in its bar: 1 on the downbeat, 0.6 on the other strong beat
    (6/8: the 4th eighth; 4/4: beat 3; 12/8: the 7th eighth), 0.3 on other beats, 0.12 on subdivisions, else 0"""
    num, den = meter_of(meter)
    bar = num * 4 / den
    compound = den == 8 and num % 3 == 0 and num > 3
    unit = 1.5 if compound else 4 / den                          # compound meters count dotted quarters
    pos = beat % bar
    on = lambda g: abs(pos / g - round(pos / g)) * g < 0.03
    beats = round(bar / unit)
    if on(bar):
        return 1.0
    if beats % 2 == 0 and on(bar / 2):
        return 0.6
    if on(unit):
        return 0.3
    return 0.12 if on(unit / (3 if compound else 2)) else 0.0


def _phrases(xs, bar, rest, long, breaks):
    starts = [(b - 1) * bar for b in breaks]
    groups = [[xs[0]]]
    for prev, x in zip(xs, xs[1:]):
        gap = x["beat"] - prev["beat"] - prev["dur"]
        if gap >= rest - 1e-6 or prev["dur"] >= long - 1e-6 or any(prev["beat"] < s - 1e-6 <= x["beat"] for s in starts):
            groups.append([])
        groups[-1].append(x)
    return groups


def phrase(notes, meter, accent=6, arc=8, soften=8, rit=0.08, breath=0.0, vary=2, seed=0, rest=0.5, long=None, breaks=()):
    """Shape a melodic line (note dicts {"beat", "dur", "pitch", "vel"}, absolute beats) like a player would:
      accent  velocity points for the metre (6/8: strong on 1 and 4, 4/4: on 1 and 3; off-beats a touch softer)
      arc     up to this many points rising to each phrase's high point, falling away after it
      soften  points off the last note of each phrase (a third of that off the one before)
      rit     ritardando: the time into a phrase's last two notes stretches by rit/2 and rit (at most 0.12 beat in
              all), legato notes stretch to meet them and the last note grows by `rit`; 0 = strict time
      breath  beats of silence before each new phrase (for winds); 0 = none
      vary    +- velocity points of seeded random variation (deterministic for a given seed)
    Phrases end at rests of at least `rest` beats, after notes of at least `long` beats (default: one bar) and
    before the bars in `breaks`. Returns new note dicts (extra keys are kept)."""
    if not notes:
        return []
    num, den = meter_of(meter)
    bar = num * 4 / den
    xs = sorted((dict(x) for x in notes), key=lambda x: (x["beat"], x["pitch"]))
    groups = _phrases(xs, bar, rest, bar if long is None else long, breaks)
    rng = random.Random(seed)
    for gi, g in enumerate(groups):
        t0, t1 = g[0]["beat"], g[-1]["beat"]
        tp = g[max(range(len(g)), key=lambda i: (g[i]["pitch"], g[i]["dur"], -i))]["beat"]
        for i, x in enumerate(g):
            t = x["beat"]
            if len(g) == 1:
                f = 0.5
            elif t <= tp:                                           # rising from 0.3 to the high point
                f = 0.3 + 0.7 * ((t - t0) / (tp - t0) if tp > t0 else 1)
            else:                                                   # falling away to the end
                f = (t1 - t) / (t1 - tp)
            x["vel"] +=accent * (metric_weight(t, meter) - 0.3) + arc * f
            if len(g) > 1 and i >= len(g) - 2:
                x["vel"] -= soften * (1 if i == len(g) - 1 else 0.33)
            x["vel"] += rng.uniform(-vary, vary) if vary else 0
        if rit and len(g) > 1:
            last = g[-1]
            end = last["beat"] + last["dur"]
            nxt = groups[gi + 1][0]["beat"] if gi + 1 < len(groups) else None
            cap = end if nxt is not None and end >= nxt else (nxt - 0.02 if nxt is not None else float("inf"))
            # the time into the last two notes stretches by rit/2 and rit: each note lands a little later
            tail = g[-3:]
            ioi = [y["beat"] - x["beat"] for x, y in zip(tail, tail[1:])]
            k = [rit / 2, rit][-len(ioi):]
            shift = [0.0]
            for i_, s in zip(ioi, k):
                shift.append(shift[-1] + s * i_)
            scale = min(1.0, 0.12 / shift[-1]) if shift[-1] > 0 else 1.0     # at most 0.12 beat late
            for x, y, s0, s1 in zip(tail, tail[1:], shift, shift[1:]):
                if x["beat"] + x["dur"] >= y["beat"] - 0.02:                 # legato notes keep meeting the next
                    x["dur"] += (s1 - s0) * scale
                y["beat"] += s1 * scale
            last["dur"] = max(0.05, min(last["dur"] * (1 + rit), cap - last["beat"]))
    if breath:
        for g, h in zip(groups, groups[1:]):
            x = g[-1]
            if x["beat"] + x["dur"] > h[0]["beat"] - breath:
                x["dur"] = max(0.4 * x["dur"], h[0]["beat"] - breath - x["beat"])
    out = [dict(x, **n(x["beat"], x["dur"], x["pitch"], x["vel"])) for g in groups for x in g]
    return sorted(out, key=lambda x: (x["beat"], x["pitch"]))


def swell_cc(notes, lo=40, hi=100, long=1.5, step=0.25, rest=1.0):
    """CC1 (expression) points for a sustained instrument playing `notes`: the level follows the line (higher and
    louder notes get more), notes of `long` beats or more swell up through their first 60% and ease back, and the
    level sinks to `lo` in rests of `rest` beats or more. Sampled every `step` beats, in cc() format."""
    xs = sorted(notes, key=lambda x: (x["beat"], -x["pitch"]))
    if not xs:
        return []
    p0, p1 = min(x["pitch"] for x in xs), max(x["pitch"] for x in xs)
    v0, v1 = min(x["vel"] for x in xs), max(x["vel"] for x in xs)
    span = hi - lo
    norm = lambda v, a, b: (v - a) / (b - a) if b > a else 0.5
    keys = []
    for i, x in enumerate(xs):
        t, d = x["beat"], x["dur"]
        L = lo + span * (0.25 + 0.45 * norm(x["pitch"], p0, p1) + 0.3 * norm(x["vel"], v0, v1))
        if d >= long:
            keys += [(t, L - 0.2 * span), (t + 0.6 * d, min(hi, L + 0.1 * span)), (t + d, L - 0.05 * span)]
        else:
            keys += [(t, L), (t + 0.9 * d, L)]
        nxt = xs[i + 1]["beat"] if i + 1 < len(xs) else None
        if nxt is None or nxt - t - d >= rest:
            gap = 1.0 if nxt is None else nxt - t - d
            keys.append((t + d + min(0.5, gap / 2), lo))
            if nxt is not None:
                keys.append((nxt - min(0.25, gap / 2), lo))
    keys.sort(key=lambda k: k[0])
    pts = []
    for (b0, a), (b1, b) in zip(keys, keys[1:]):
        pts += ramp(b0, a, b1, b, step)[:-1] if b1 - b0 > 1e-6 else []
    pts.append(cc(*keys[-1]))
    out = {}
    for p in pts:
        out[p["beat"]] = p                                          # one point per beat, the later one wins
    return [out[b] for b in sorted(out)]
