"""Made-up songs for the lint parity test, with tools/score_lint.py's report on each, packed into
daw/crates/flux-score/tests/data/lint_reference.json.

    .venv/Scripts/python -I daw/crates/flux-score/tests/make/make_lint_reference.py <repo root> <out.json>

Two sketches from fixed seeds, neither quoting any song: melodies are random walks over a scale and the harmony
is a random progression. "sketch_68" (6/8) has orchestral parts, suspensions and a low passage for the mud check;
"sketch_44" (4/4) has doubled guitars (merged findings), a pitch-bent lead (including bends that settle too late
to count and one exactly at the settle point), keyswitch blips and slightly early or late piano chords. Bars
without a chord are N.C., and "X|Y" splits a bar evenly, as score_lint.py reads a build script's HARM.

No chord holds a minor 2nd (no maj7): Flux hears a major 7th between two tones of the named chord as the chord's
colour, which score_lint.py reports as a clash. Flux's own lint tests cover that.
"""
import json
import os
import random
import subprocess
import sys
import tempfile
import types


def load_score(root):
    sys.path.insert(0, os.path.join(root, "daw", "bridge"))
    stub = types.ModuleType("live")
    stub.PORT = 0
    stub.send = lambda *a, **k: (_ for _ in ()).throw(RuntimeError("offline"))
    sys.modules["live"] = stub
    import score
    return score


class Song:
    def __init__(self, score, name, meter, bars, harm):
        self.score, self.name, self.meter, self.bars, self.harm = score, name, meter, bars, harm
        self.bar_len = meter[0] * 4 / meter[1]
        self.tracks = {}

    def track(self, name, path):
        return self.tracks.setdefault(name, {"path": path, "notes": [], "bends": []})

    def note(self, track, start, dur, pitch, vel):
        track["notes"].append({"start": round(start, 4), "dur": round(dur, 4), "pitch": int(pitch), "vel": int(vel)})

    def parts(self, bar):
        """the chords of a bar: [(offset, length, symbol)], or [] for N.C."""
        v = self.harm.get(bar)
        if v is None:
            return []
        syms = v.split("|")
        n = self.bar_len / len(syms)
        return [(i * n, n, s) for i, s in enumerate(syms)]

    def chord_at(self, bar, off):
        for o, n, s in self.parts(bar):
            if o - 1e-9 <= off < o + n - 1e-9:
                return s
        return None

    def pcs(self, sym):
        return set(self.score.chord(sym).pcs)

    def bass_pc(self, sym):
        return self.score.chord(sym).bass

    def project(self):
        return {
            "name": self.name, "tempo": [{"bar": 1, "bpm": 100.0}], "time_signature": list(self.meter), "bars": self.bars,
            "tracks": [{"name": n, "instrument": {"kind": "sfz", "path": t["path"]}, "notes": t["notes"],
                        "bends": sorted(t["bends"], key=lambda b: b["beat"])} for n, t in self.tracks.items()],
        }

    def marks(self):
        out = []
        for bar in range(1, self.bars + 1):
            ps = self.parts(bar)
            out += [[bar, o, s] for o, _, s in ps] if ps else [[bar, 0.0, "N.C."]]
        return out


def in_range(pcs, lo, hi):
    return [p for p in range(lo, hi + 1) if p % 12 in pcs]


def nearest(p, pcs):
    return min((q for q in range(p - 6, p + 7) if q % 12 in pcs), key=lambda q: (abs(q - p), q))


def index_near(pitches, p):
    return min(range(len(pitches)), key=lambda i: (abs(pitches[i] - p), i))


def progression(rng, pool, bars, split, skip):
    harm = {}
    for b in range(1, bars + 1):
        if b in skip:
            continue
        harm[b] = f"{rng.choice(pool)}|{rng.choice(pool)}" if rng.random() < split else rng.choice(pool)
    return harm


def melody(song, rng, track, scale, lo, hi, start, rhythms, strong, rest_every, vel=(64, 100), snap=0.6):
    """a random walk over the scale, snapped to a chord tone on most strong beats; returns [(start, dur, pitch)]"""
    pitches = in_range(scale, lo, hi)
    i = index_near(pitches, start)
    line = []
    for bar in range(1, song.bars + 1):
        if rest_every and bar % rest_every == 0:
            continue
        t0 = (bar - 1) * song.bar_len
        off = 0.0
        for d in rng.choice(rhythms):
            i = max(0, min(len(pitches) - 1, i + rng.choice([-2, -1, -1, 0, 1, 1, 2, 3, -3])))
            p = pitches[i]
            sym = song.chord_at(bar, off)
            if sym and off in strong and rng.random() < snap:
                p = nearest(p, song.pcs(sym))
                i = index_near(pitches, p)
            dur = d * rng.choice([1.0, 1.0, 0.9])
            song.note(track, t0 + off, dur, p, rng.randint(*vel))
            line.append((t0 + off, dur, p))
            off += d
    return line


def sketch_68(score):
    rng = random.Random(6808)
    pool = ["G", "C", "D", "Em", "Am", "C/G", "F/G", "Dsus", "Bm", "G/B", "D/F#", "Am7", "F"]
    song = Song(score, "sketch_68", (6, 8), 32, progression(rng, pool, 32, 0.2, skip={32}))
    scale = {7, 9, 11, 0, 2, 4, 5}                                  # G mixolydian
    L = song.bar_len
    rhythms = [[1.5, 1.5], [1.0, 0.5, 1.5], [0.5, 0.5, 0.5, 1.5], [3.0], [1.5, 1.0, 0.5], [0.5, 1.0, 1.5]]
    flute = melody(song, rng, song.track("Flute", "build/sfz/sketch/flute.sfz"), scale, 67, 86, 74, rhythms,
                   (0.0, 1.5), rest_every=8)

    # a counter-line a third under the flute in bars 9-24, sometimes only a second under
    clar = song.track("Clarinet", "build/sfz/sketch/clarinet.sfz")
    low = in_range(scale, 52, 84)
    for s, d, p in flute:
        if 8 * L <= s < 24 * L:
            k = index_near(low, p) - (1 if rng.random() < 0.15 else 2)
            song.note(clar, s, d, low[max(0, k)], rng.randint(60, 90))

    harp, violas = song.track("Harp", "build/sfz/sketch/harp.sfz"), song.track("Violas", "build/sfz/sketch/violas.sfz")
    celli, basses = song.track("Celli", "build/sfz/sketch/celli.sfz"), song.track("Basses", "build/sfz/sketch/basses.sfz")
    winds = song.track("Low Winds", "build/sfz/sketch/bassoons.sfz")
    held = None
    for bar in range(1, song.bars + 1):
        t0 = (bar - 1) * L
        for o, n, sym in song.parts(bar):
            pcs = song.pcs(sym)
            tones = in_range(pcs, 55, 76)
            steps = int(n / 0.5)
            for k in range(steps):                                # arpeggio, left to ring
                song.note(harp, t0 + o + k * 0.5, 1.0 if k < steps - 1 else 0.5, tones[[0, 1, 2, 3, 2, 1][k % 6] % len(tones)],
                          rng.randint(55, 85))
            if held is not None and rng.random() < 0.15:        # hold the last chord's note over the change
                v = held
            else:
                v = nearest(62, pcs - {score.chord(sym).root % 12} or pcs)
            song.note(violas, t0 + o, n, v, rng.randint(50, 75))
            held = v
            song.note(celli, t0 + o, n * 0.95, nearest(47, {score.chord(sym).root % 12}), rng.randint(55, 80))
            song.note(basses, t0 + o, n, nearest(36, {song.bass_pc(sym)}), rng.randint(60, 85))
            if 21 <= bar <= 24:                                   # a low, thick passage
                for q in sorted(in_range(pcs, 38, 47))[:3]:
                    song.note(winds, t0 + o, n, q, rng.randint(50, 70))
    drum = song.track("Drum", "sfz/sketch_perc.sfz")
    for bar in range(1, song.bars + 1):
        for off, p in ((0.0, 36), (1.5, 38), (2.5, 42)):
            song.note(drum, (bar - 1) * L + off, 0.25, p, rng.randint(70, 110))
    return song


def sketch_44(score):
    rng = random.Random(4404)
    pool = ["Em", "C", "D", "G", "Am", "B", "B7", "Bm", "D/F#", "Am7", "Esus", "G/B", "A"]
    song = Song(score, "sketch_44", (4, 4), 40, progression(rng, pool, 40, 0.4, skip={1, 2}))
    scale = {4, 6, 7, 9, 11, 0, 2}                                 # E natural minor
    L = song.bar_len
    rhythms = [[1, 1, 2], [2, 2], [0.5, 0.5, 1, 2], [1.5, 0.5, 2], [4], [1, 1, 1, 1], [0.5, 0.5, 0.5, 0.5, 2]]
    lead_t = song.track("Lead", "sfz/rigs/sketch_lead.rig.json")
    lead = melody(song, rng, lead_t, scale, 64, 88, 71, rhythms, (0.0, 2.0), rest_every=0, vel=(80, 115))
    lead = [x for x in lead if x[0] >= 2 * L]
    lead_t["notes"] = [n for n in lead_t["notes"] if n["start"] >= 2 * L]
    # bends: up a whole step or a half step, a vibrato too small to change the pitch, one that settles too late to
    # count, and one exactly at the settle point; each returns to zero when the note ends
    exact = False
    for s, d, p in lead:
        if d < 2 or rng.random() < 0.4:
            continue
        kind = rng.choice(["whole", "half", "vib", "late"]) if exact else "exact"
        at, value = {"whole": (s, 8191), "half": (s, 4096), "vib": (s, 1500), "late": (s + 0.5, 8191),
                     "exact": (s + min(0.3, d / 2), -8192)}[kind]
        exact = True
        lead_t["bends"] += [{"beat": round(at, 4), "value": value}, {"beat": round(s + d, 4), "value": 0}]

    harmony = song.track("Harmony", "sfz/rigs/sketch_lead.rig.json")
    low = in_range(scale, 52, 86)
    for s, d, p in lead:
        k = index_near(low, p) - (1 if rng.random() < 0.12 else 2)
        song.note(harmony, s, d, low[max(0, k)], rng.randint(70, 100))

    piano, pad = song.track("Piano", "sfz/sketch_piano.sfz"), song.track("Pad", "build/sfz/sketch/pad.sfz")
    gl, gr = song.track("Gtr L", "sfz/rigs/sketch_L.rig.json"), song.track("Gtr R", "sfz/rigs/sketch_R.rig.json")
    bass, sub = song.track("Bass", "sfz/sketch_bass.sfz"), song.track("Sub", "sfz/sketch_sub.sfz")
    cello = song.track("Cello", "build/sfz/sketch/cello.sfz")
    for bar in range(1, song.bars + 1):
        t0 = (bar - 1) * L
        for o, n, sym in song.parts(bar):
            pcs, root = song.pcs(sym), score.chord(sym).root % 12
            for k, q in enumerate(sorted(in_range(pcs, 52, 71))[:4]):
                jitter = round(rng.uniform(-0.02, 0.02), 3) if k == 0 else 0.0
                song.note(piano, max(0.0, t0 + o + jitter), 1.5, q, rng.randint(60, 90))
            for q in sorted(in_range(pcs, 60, 76))[:2]:
                song.note(pad, t0 + o, n, q, rng.randint(40, 60))
            r = nearest(45, {root})
            for k in range(int(n / 0.5)):
                for g in (gl, gr) if bar > 8 else ():
                    for q in (r, r + 7, r + 12):
                        song.note(g, t0 + o + k * 0.5, 0.5, q, 100)
                song.note(bass, t0 + o + k * 0.5, 0.45, nearest(33, {song.bass_pc(sym)}), rng.randint(80, 100))
            if 33 <= bar <= 36:
                for q in sorted(in_range(pcs, 33, 45))[:3]:
                    song.note(sub, t0 + o, n, q, 90)
        if bar >= 25:
            song.note(cello, t0, 0.02, 24, 1)                      # keyswitch blip
            song.note(cello, t0 + 0.05, rng.choice([2, 3, 3.95]), rng.choice(in_range(scale, 43, 57)), rng.randint(60, 85))
    kit = "sfz/sketch_kit.sfz"
    kick, snare, cym = song.track("Kick", kit), song.track("Snare", kit), song.track("Cymbals", kit)
    for bar in range(1, song.bars + 1):
        t0 = (bar - 1) * L
        for off in (0, 2.5):
            song.note(kick, t0 + off, 0.25, 36, 110)
        for off in (1, 3):
            song.note(snare, t0 + off, 0.25, 38, 105)
        for k in range(8):
            song.note(cym, t0 + k * 0.5, 0.25, 42, 80)
    return song


def lint(root, song, tmp):
    project = os.path.join(tmp, f"{song.name}.flux.json")
    script = os.path.join(tmp, f"{song.name}_harm.py")
    json.dump(song.project(), open(project, "w", encoding="utf-8"))
    with open(script, "w", encoding="utf-8") as f:
        f.write(f"HARM = {song.harm!r}\n")
    out = subprocess.run([sys.executable, "-I", os.path.join(root, "tools", "score_lint.py"), project, "--harmony", script,
                          "--catalog", os.path.join(tmp, "no-cards"), "--max", "500"],
                         capture_output=True, text=True, encoding="utf-8", check=True).stdout
    lines = out.splitlines()
    start = next(i for i, l in enumerate(lines) if l.startswith("  found:"))
    return lines, lines[start:]


def main(root, out_path):
    score = load_score(root)
    songs = []
    with tempfile.TemporaryDirectory() as tmp:
        for song in (sketch_68(score), sketch_44(score)):
            full, found = lint(root, song, tmp)
            print("\n".join(full[:8]), f"\n  ... {len(found)} lines from 'found:'\n")
            songs.append({
                "name": song.name, "time_signature": list(song.meter), "bars": song.bars, "harmony": song.marks(),
                "tracks": [{"name": n, "path": t["path"], "notes": [[x["start"], x["dur"], x["pitch"], x["vel"]] for x in t["notes"]],
                            "bends": [[b["beat"], b["value"]] for b in sorted(t["bends"], key=lambda b: b["beat"])]}
                           for n, t in song.tracks.items()],
                "python": found,
            })
    json.dump({"songs": songs}, open(out_path, "w", encoding="utf-8"), separators=(",", ":"))
    print(out_path, sum(len(t["notes"]) for s in songs for t in s["tracks"]), "notes")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
