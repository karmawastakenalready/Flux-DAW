"""Lint a finished Flux project for things a listener would hear as mistakes. Reads the project only; never talks to Flux.

    python -I tools/score_lint.py daw/projects/far_roads.flux.json [--harmony daw/projects/far_roads_build.py]
           [--melody "Flute,Oboe"] [--transpose "Cello=12"] [--catalog DIR] [--max 40]

  clash  notes in two tracks sounding together for at least half a beat a minor 2nd, major 7th or minor 9th apart,
         unless either is a short passing note (under a beat, moving by step in its own line)
  nct    melody notes on strong beats (6/8: 1 and 4; 4/4: 1 and 3) that aren't chord tones: an appoggiatura if it
         resolves by step to a chord tone, else a clash. Needs --harmony: the build script's HARM (bar -> chord,
         "C|D" = two halves), names resolved through the script's CH where it has one, else score.chord
  range  notes outside the playable keys of catalog/*.json instrument cards, matched by SFZ path (skipped without)
  mud    more than 3 different sustained pitches below C3 within an octave at once
Melody = the top line of each pitched track whose top line lies mostly at C4 or above (or the --melody tracks).
Unpitched tracks (drums, percussion, clock, industrial, fx: by name or SFZ path) and keyswitch blips are ignored.
Pitch bends count (+-2 semitones assumed); --transpose maps off-pitch instruments (key + n = sounding) for every
check but range. Repeated findings are merged ("also bars ..."); everything is listed under the bar where it first happens.
"""
import argparse
import bisect
import glob
import importlib.util
import json
import os
import re
import sys
import types
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "daw", "bridge"))
_stub = types.ModuleType("live")                       # build scripts and score import live: give them a dead one
_stub.PORT = 0
_stub.send = lambda *a, **k: (_ for _ in ()).throw(RuntimeError("score_lint never talks to Flux"))
sys.modules["live"] = _stub
import score  # noqa: E402
from score import chord, metric_weight, note_name  # noqa: E402

UNPITCHED = re.compile(r"drum|kick|snare|\btoms?\b|cymbal|hi-?hat|perc|clock|industrial|noise|\bfx\b", re.I)
INTERVALS = {1: "minor 2nd", 11: "major 7th", 13: "minor 9th"}
MIN_DUR = 0.05            # shorter notes are keyswitches
TOL = 0.03                # beats of humanize slop when matching grid positions
BEND_RANGE = 2            # semitones at full pitch bend (+-8192)


def load(path, transpose=None):
    """the project and its tracks: `keys` as stored, `notes` as heard (pitch bends applied: the bend settled a little
    into each note decides its pitch; `transpose` = {track: semitones} for instruments mapped off their sounding pitch)"""
    proj = json.load(open(path, encoding="utf-8"))
    tracks = []
    for t in proj["tracks"]:
        inst = t.get("instrument") or {}
        keys = sorted((x["start"], x["dur"], x["pitch"], x["vel"]) for x in t.get("notes", []) if x["dur"] >= MIN_DUR)
        bends = sorted(t.get("bends") or [], key=lambda b: b["beat"])
        beats = [b["beat"] for b in bends]

        shift = (transpose or {}).get(t["name"], 0)

        def heard(s, d, p, v):
            i = bisect.bisect_right(beats, s + min(0.3, d / 2) + 1e-9)
            return s, d, p + shift + (round(bends[i - 1]["value"] / 8192 * BEND_RANGE) if i else 0), v
        pitched = not UNPITCHED.search(f'{t["name"]} {inst.get("path", "")}')
        tracks.append({"name": t["name"], "path": inst.get("path") or "", "keys": keys, "pitched": pitched,
                       "notes": sorted(heard(*x) for x in keys) if bends or shift else keys, "bent": bool(bends)})
    return proj, tracks


def top_line(notes):
    """a track's melody: the highest note of each onset (within TOL), skipping notes that start more than an octave
    under a line note still sounding (the accompaniment under a held melody note)"""
    line = []
    for s, d, p, v in notes:
        if line and s - line[-1][0] < TOL:
            if p > line[-1][2]:
                line[-1] = (line[-1][0], d, p, v)
        elif not (line and line[-1][0] + line[-1][1] > s + 0.25 and p < line[-1][2] - 12):
            line.append((s, d, p, v))
    return line


def passing(notes):
    """indices of short notes (under a beat) that move by step to or from a note at the neighbouring onset"""
    onsets = sorted({round(s, 2) for s, _, _, _ in notes})
    at = defaultdict(list)
    for s, d, p, v in notes:
        at[round(s, 2)].append(p)
    idx = {o: i for i, o in enumerate(onsets)}
    out = set()
    for k, (s, d, p, v) in enumerate(notes):
        if d >= 1.0:
            continue
        i = idx[round(s, 2)]
        near = [q for j in (i - 1, i + 1) if 0 <= j < len(onsets) and abs(onsets[j] - s) <= max(d, 0.5) + 0.25
                for q in at[onsets[j]]]
        if any(1 <= abs(p - q) <= 2 for q in near):
            out.add(k)
    return out


class Report:
    def __init__(self, bar_len):
        self.bar_len, self.items = bar_len, {}

    def bar(self, t):
        return int((t + TOL) // self.bar_len) + 1

    def add(self, kind, t, key, text):
        """one finding; the same `key` in other bars only adds the bar"""
        b = self.bar(t)
        it = self.items.setdefault((kind, key), {"kind": kind, "bars": [], "text": text})
        if b not in it["bars"]:
            it["bars"].append(b)

    def count(self, kind):
        return sum(len(it["bars"]) for it in self.items.values() if it["kind"] == kind)


def spans(bars):
    out, run = [], [bars[0]]
    for b in bars[1:]:
        if b == run[-1] + 1:
            run.append(b)
        else:
            out.append(run)
            run = [b]
    out.append(run)
    return ", ".join(f"{r[0]}-{r[-1]}" if len(r) > 1 else str(r[0]) for r in out)


# ---------------------------------------------------------------------------------------------
# checks

def check_clashes(tracks, rep):
    ev = []
    for ti, t in enumerate(tracks):
        if t["pitched"]:
            short = passing(t["notes"])
            ev += [(s, d, p, ti, k in short) for k, (s, d, p, v) in enumerate(t["notes"])]
    ev.sort()
    active = []
    found = defaultdict(lambda: [set(), 0.0])            # (bar, high track, high pitch, low pitch) -> low tracks, overlap
    for s, d, p, ti, short in ev:
        active = [a for a in active if a[0] + a[1] >= s + 0.5]
        for a in active:
            iv = abs(p - a[2])
            if a[3] == ti or iv not in INTERVALS or short or a[4]:
                continue
            ov = min(s + d, a[0] + a[1]) - max(s, a[0])
            if ov >= 0.5:
                (lo, lo_p), (hi, hi_p) = sorted(((a[3], a[2]), (ti, p)), key=lambda x: x[1])
                f = found[(rep.bar(s), tracks[hi]["name"], hi_p, lo_p)]
                f[0].add(tracks[lo]["name"])
                f[1] = max(f[1], ov)
        active.append((s, d, p, ti, short))
    merged = defaultdict(lambda: [set(), 0.0])           # doubled parts (Gtr L + Gtr R) share one line
    for (bar, hi_t, hi_p, lo_p), (los, ov) in found.items():
        m = merged[(bar, hi_p, lo_p, ", ".join(sorted(los)))]
        m[0].add(hi_t)
        m[1] = max(m[1], ov)
    for (bar, hi_p, lo_p, los), (his, ov) in merged.items():
        his = ", ".join(sorted(his))
        rep.add("clash", (bar - 1) * rep.bar_len, (his, hi_p, los, lo_p),
                f"{his} {note_name(hi_p)} against {los} {note_name(lo_p)}: {INTERVALS[hi_p - lo_p]}, {ov:.1f} beats")


def load_harmony(script):
    spec = importlib.util.spec_from_file_location("lint_build", os.path.abspath(script))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    harm = getattr(mod, "HARM", None)
    if not harm:
        return None, "no HARM in the build script"
    ch = getattr(mod, "CH", {})

    def tones(name):
        pcs = set()
        if isinstance(ch, dict) and isinstance(ch.get(name), dict):
            for v in ch[name].values():
                pcs |= {x % 12 for x in (v if isinstance(v, (list, tuple)) else [v]) if isinstance(x, int)}
        try:
            c = chord(name)
            pcs |= set(c.pcs) | {c.bass}
        except ValueError:
            pass
        return pcs

    out = {}
    for bar, val in harm.items():
        parts = val.split("|") if isinstance(val, str) else list(val)
        out[bar] = [(p if isinstance(p, str) else chord(p).name, tones(p) if isinstance(p, str) else set(chord(p).pcs))
                    for p in parts]
    return out, os.path.basename(script)


def check_harmony(tracks, harm, meter, rep, melody):
    bar_len = rep.bar_len
    num, den = score.meter_of(meter)

    def chord_at(t):
        b = rep.bar(t)
        if b not in harm:
            return None
        parts, part = harm[b], bar_len / len(harm[b])
        i = min(len(parts) - 1, int((max(0.0, t - (b - 1) * bar_len) + TOL) // part))
        return parts[i] + ((b - 1) * bar_len + (i + 1) * part,)            # (name, tones, end of the chord)

    for t in tracks:
        if t["name"] not in melody:
            continue
        line = top_line(t["notes"])
        for i, (s, d, p, v) in enumerate(line):
            c = chord_at(s)
            if c is None or metric_weight(s, meter) < 0.6 or p % 12 in c[1]:
                continue
            count = round((s - (rep.bar(s) - 1) * bar_len) / (4 / den)) + 1
            later = [q for q in line[i + 1:] if q[0] <= s + d + 0.5]              # the line while the note fades
            res = next((q for q in later if 1 <= abs(q[2] - p) <= 2 and q[0] < c[2] - TOL and q[2] % 12 in c[1]), None)
            nxt = next((q for q in later if q[0] >= s + d - 0.25), None)          # the note after it
            near = min(min((p - q) % 12, (q - p) % 12) for q in c[1])
            rub = {1: "a semitone", 2: "a whole step"}.get(near, f"{near} semitones")
            step = None if not nxt else abs(nxt[2] - p)
            move = ("not resolved" if not nxt else "repeated" if step == 0 else
                    f"{'steps' if step <= 2 else 'leaps'} to {note_name(nxt[2])}" + (" after the chord changes" if nxt[0] >= c[2] - TOL else ""))
            how = f"appoggiatura -> {note_name(res[2])}" if res else f"clash ({rub} from a chord tone), {move}"
            rep.add("nct-app" if res else "nct-clash", s, (t["name"], p, c[0], how),
                    f"{t['name']} {note_name(p)} on {count} over {c[0]}: {how}")


def find_range(card):
    """(lo, hi) playable keys from an instrument card, whatever its exact layout"""
    def val(x):
        if isinstance(x, str):
            try:
                return score.P(x)
            except (TypeError, AttributeError, KeyError):
                return None
        return int(x) if isinstance(x, (int, float)) and not isinstance(x, bool) else None

    def pair(v):
        if isinstance(v, (list, tuple)) and len(v) == 2:
            lo, hi = val(v[0]), val(v[1])
        elif isinstance(v, dict):
            lo = next((val(v[k]) for k in ("lo", "low", "min", "lokey", "from", "first") if k in v), None)
            hi = next((val(v[k]) for k in ("hi", "high", "max", "hikey", "to", "last") if k in v), None)
        else:
            return None
        return (lo, hi) if lo is not None and hi is not None and lo <= hi else None

    def walk(d, depth=0):
        if not isinstance(d, dict) or depth > 3:
            return None
        for k in sorted(d, key=lambda k: ("playable" not in k.lower(), "range" not in k.lower())):
            if any(w in k.lower() for w in ("playable", "range", "keys")):
                r = pair(d[k])
                if r:
                    return r
        for v in d.values():
            r = walk(v, depth + 1)
            if r:
                return r
        return None
    return walk(card)


def norm_path(p):
    p = os.path.normpath(p) if p else ""
    if os.path.isabs(p):
        try:
            p = os.path.relpath(p, ROOT)
        except ValueError:
            pass
    return p.replace("\\", "/").lower()


def load_catalog(folder):
    cards = {}
    for f in sorted(glob.glob(os.path.join(folder, "*.json"))):
        try:
            card = json.load(open(f, encoding="utf-8"))
        except (OSError, ValueError):
            continue
        path = next((card[k] for k in ("sfz", "sfz_path", "path", "file", "source") if isinstance(card.get(k), str)), None)
        if path is None and isinstance(card.get("instrument"), dict):
            path = card["instrument"].get("path")
        rng = find_range(card)
        if path and rng:
            cards[norm_path(path)] = (rng, os.path.basename(f))
    return cards


def check_range(tracks, cards, rep):
    matched = []
    for t in tracks:
        key = norm_path(t["path"])
        hit = cards.get(key) or next((v for k, v in cards.items() if key and (k.endswith(key) or key.endswith(k))), None)
        if not hit:
            continue
        (lo, hi), card = hit
        matched.append(t["name"])
        for s, d, p, v in t["notes"]:
            if not lo <= p <= hi:
                side = "below" if p < lo else "above"
                rep.add("range", s, (t["name"], p),
                        f"{t['name']} {note_name(p)} is {side} its playable {note_name(lo)}-{note_name(hi)} ({card})")
    return matched


def check_mud(tracks, rep, floor=48, sustain=1.0):
    ev = [(s, s + d, p, t["name"]) for t in tracks if t["pitched"] for s, d, p, v in t["notes"] if p < floor and d >= sustain]
    times = sorted({x for s, e, _, _ in ev for x in (s, e)})
    for a, b in zip(times, times[1:]):
        if b - a < 0.25:
            continue
        on = [(p, nm) for s, e, p, nm in ev if s <= a + 1e-6 and e >= b - 1e-6]
        ps = sorted({p for p, _ in on})
        for i in range(len(ps)):
            group = [q for q in ps[i:] if q - ps[i] <= 12]
            if len(group) > 3:
                who = sorted({nm for p, nm in on if p in group})
                rep.add("mud", a, tuple(group), f"{' '.join(note_name(q) for q in group)} at once ({', '.join(who)})")
                break


# ---------------------------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("project")
    ap.add_argument("--harmony", help="the song's build script (its HARM and CH)")
    ap.add_argument("--melody", help="comma-separated melody tracks (default: guessed)")
    ap.add_argument("--transpose", help='sounding offsets of off-pitch instruments, e.g. "Cello=12,Bass=-12"')
    ap.add_argument("--catalog", default=os.path.join(ROOT, "catalog"))
    ap.add_argument("--max", type=int, default=40, help="findings to list (the summary counts all)")
    a = ap.parse_args()

    shift = {k.strip(): int(v) for k, v in (x.split("=") for x in a.transpose.split(","))} if a.transpose else {}
    proj, tracks = load(a.project, shift)
    num, den = proj.get("time_signature") or (4, 4)
    rep = Report(num * 4 / den)
    pitched = [t for t in tracks if t["pitched"] and t["notes"]]
    if a.melody:
        melody = [m.strip() for m in a.melody.split(",")]
    else:
        tops = {t["name"]: sorted(p for _, _, p, _ in top_line(t["notes"])) for t in pitched}
        melody = [nm for nm, ps in tops.items() if ps[len(ps) // 2] >= 60]

    check_clashes(tracks, rep)
    check_mud(tracks, rep)
    harm, hnote = (None, "no --harmony") if not a.harmony else load_harmony(a.harmony)
    if harm:
        check_harmony(tracks, harm, (num, den), rep, melody)
    if os.path.isdir(a.catalog):
        cards = load_catalog(a.catalog)
        matched = check_range(tracks, cards, rep)
        rnote = f"{len(cards)} cards, {len(matched)} of {len(pitched)} pitched tracks matched"
    else:
        rnote = f"skipped: no {os.path.relpath(a.catalog, ROOT)}/"

    print(f"{os.path.basename(a.project)}: {num}/{den}, {proj.get('bars')} bars, {len(pitched)} pitched tracks")
    print(f"  melody: {', '.join(melody) or '-'}")
    print(f"  harmony: {hnote if not harm else hnote + f' ({len(harm)} bars)'}")
    print(f"  range: {rnote}")
    if shift:
        print(f"  transposed to sounding pitch: {', '.join(f'{k} {v:+d}' for k, v in shift.items())}")
    if any(t["bent"] for t in pitched):
        print(f"  pitch bends applied (+-{BEND_RANGE} semitones) on: {', '.join(t['name'] for t in pitched if t['bent'])}")
    napp, ncl = rep.count("nct-app"), rep.count("nct-clash")
    print(f"  found: {rep.count('clash')} sustained clashes, "
          + (f"{napp + ncl} strong-beat non-chord tones ({napp} appoggiaturas, {ncl} clashes), " if harm else "")
          + f"{rep.count('range')} out-of-range notes, {rep.count('mud')} muddy spots (counted per bar)")
    items = sorted(rep.items.values(), key=lambda it: (it["bars"][0], it["kind"], it["text"]))
    label = {"clash": "clash", "nct-app": "nct", "nct-clash": "nct", "mud": "mud", "range": "range"}
    last = None
    for it in items[:a.max]:
        b = it["bars"][0]
        more = f"   (also bars {spans(it['bars'][1:])})" if len(it["bars"]) > 1 else ""
        print(f"{'bar ' + str(b) if b != last else '':>8}  {label[it['kind']]:<6}{it['text']}{more}")
        last = b
    if len(items) > a.max:
        print(f"  ... {len(items) - a.max} more (--max)")


if __name__ == "__main__":
    main()
