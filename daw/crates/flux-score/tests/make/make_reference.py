"""Reference outputs of daw/bridge/score.py for the flux-score parity tests, written to
daw/crates/flux-score/tests/data/python_reference.json.

    .venv/Scripts/python -I daw/crates/flux-score/tests/make/make_reference.py <repo root> <out.json>

score.py imports `live` (the session bridge); a stub stands in for it. Every input is made up: the two long
progressions are random walks over a song-like pool (slash chords, sus chords, bars split in two), not any song's.
"""
import json
import random
import sys
import types


def main(root, out_path):
    sys.modules["live"] = types.SimpleNamespace(send=None, PORT=0)
    sys.path.insert(0, f"{root}/daw/bridge")
    import score

    ref = {}

    # ---- metric weight
    meters = ["4/4", "3/4", "2/4", "5/4", "6/8", "9/8", "12/8", "7/8", "3/8", "2/2", "6/4"]
    beats = [i / 8 for i in range(0, 8 * 13)] + [0.02, 0.97, 1.49, 1.51, 2.99, 3.01, -0.5, -1.0, 0.333, 0.6667, 7.98]
    ref["metric_weight"] = [[m, b, score.metric_weight(b, m)] for m in meters for b in beats]

    # ---- chord symbols
    roots = ["C", "F#", "Bb", "E", "Ab", "G", "Db", "B"]
    quals = list(score.QUALITY) + list(score.ALIAS)
    chords = {}
    for r in roots:
        for q in quals:
            sym = r + q
            c = score.chord(sym)
            chords[sym] = {"root": c.root, "bass": c.bass, "pcs": list(c.pcs), "roles": [c.roles[p] for p in c.pcs]}
    for sym in ["C/E", "F/G", "Am/C", "G/B", "D/F#", "Bb/D", "Em/G", "C/G", "Dsus/A", "G7/F", "Cmaj7/B", "Am7/G"]:
        c = score.chord(sym)
        chords[sym] = {"root": c.root, "bass": c.bass, "pcs": list(c.pcs), "roles": [c.roles[p] for p in c.pcs]}
    ref["chords"] = chords

    # ---- notation
    cases = [
        dict(text="B4:3 E5:3 | D5:2 C5:1 B4:2 A4:1", bpb=3, unit=0.5, bar=2, vel=80, shift=0, legato=1.0, short=0.85, arc=0),
        dict(text="G4:2 r:1 B4:1 D5:2 | C5:4 r:2", bpb=3, unit=0.5, bar=9, vel=72, shift=12, legato=0.95, short=0.7, arc=10),
        dict(text="E4:1 F#4:1 G4:2 | A4:3 r:1 | B4:4", bpb=4, unit=1, bar=1, vel=100, shift=-12, legato=1.0, short=0.85, arc=7),
        dict(text="C5:0.5 D5:0.5 E5:1 r:2 | Bb4:1.5 A4:0.5 G4:2", bpb=4, unit=1, bar=17, vel=64, shift=0, legato=0.9, short=0.5, arc=3),
        dict(text="r:4 | Eb5:3 D5:1", bpb=4, unit=1, bar=5, vel=90, shift=2, legato=1.0, short=0.85, arc=20),
        dict(text="A3:6 | r:3 C#4:3", bpb=3, unit=0.5, bar=33, vel=110, shift=0, legato=1.0, short=1.0, arc=5),
    ]
    ref["melody"] = []
    for c in cases:
        score.setup(bpb=c["bpb"], unit=c["unit"], pause=0)
        notes = score.melody(c["text"], c["bar"], c["vel"], shift=c["shift"], legato=c["legato"], short=c["short"], arc=c["arc"])
        ref["melody"].append(dict(c, notes=notes))

    # ---- progressions
    srng = random.Random(1964)
    a_pool = ["G", "C", "D", "Em", "Am", "C/G", "F/G", "Dsus", "G/B", "D/F#", "F", "Am7", "Cmaj7", "Bm"]
    a_prog = [srng.choice(a_pool) for _ in range(64)]
    b_pool = ["Em", "C", "D", "G", "Am", "B", "B7", "Bm", "Cmaj7", "Esus", "G/B", "A", "D/F#", "Am7"]
    b_prog = [s for _ in range(84) for s in (f"{srng.choice(b_pool)}|{srng.choice(b_pool)}" if srng.random() < 0.1
                                             else srng.choice(b_pool)).split("|")]
    jazz = ["Dm7", "G7", "Cmaj7", "Am7", "Dm9", "G13", "Cmaj9", "C6/9", "Fmaj7", "Bm7b5", "E7", "Am9", "D9sus", "G7sus",
            "Cmaj13", "Ebdim7", "Dm11", "G9", "C", "Cadd9"]
    rng = random.Random(20261008)
    pool = [r + q for r in ["C", "D", "E", "F", "G", "A", "B", "Bb", "Eb", "F#", "Ab", "C#"] for q in score.QUALITY]
    progs = [("sketch_a", a_prog), ("sketch_a_1_8", a_prog[:8]), ("sketch_a_25_32", a_prog[24:32]),
             ("sketch_b", b_prog), ("sketch_b_33_40", b_prog[29:38]), ("jazz", jazz)]
    for i in range(30):
        progs.append((f"random{i}", [rng.choice(pool) for _ in range(rng.randint(3, 9))]))

    def attempt(f):
        try:
            return {"ok": f()}
        except ValueError as e:
            return {"error": str(e)}

    ref["voice_lead"] = []
    ref["bass_line"] = []
    settings = [dict(voices=4, low=48, high=79), dict(voices=4, low=55, high=79), dict(voices=3, low=52, high=76),
                dict(voices=5, low=48, high=84), dict(voices=2, low=60, high=79)]
    for name, prog in progs:
        for k, s in enumerate(settings):
            if name.startswith("random") and k not in (0, (int(name[6:]) % 4) + 1):
                continue
            r = attempt(lambda: score.voice_lead(prog, **s))
            if "ok" in r:
                r = {"pitches": [list(v) for v in r["ok"].pitches], "warnings": r["ok"].warnings}
            ref["voice_lead"].append(dict(name=name, chords=prog, **s, start=None, bass=None, result=r))
        # with a bass line under the voices, and with a start voicing
        bass = attempt(lambda: score.bass_line(prog))
        if "ok" in bass:
            b = bass["ok"]
            r = attempt(lambda: score.voice_lead(prog, voices=4, low=50, high=79, bass=b))
            if "ok" in r:
                r = {"pitches": [list(v) for v in r["ok"].pitches], "warnings": r["ok"].warnings}
            ref["voice_lead"].append(dict(name=name + "+bass", chords=prog, voices=4, low=50, high=79, start=None, bass=b, result=r))
        start = [55, 59, 62, 67]
        r = attempt(lambda: score.voice_lead(prog, voices=4, low=48, high=79, start=start))
        if "ok" in r:
            r = {"pitches": [list(v) for v in r["ok"].pitches], "warnings": r["ok"].warnings}
        ref["voice_lead"].append(dict(name=name + "+start", chords=prog, voices=4, low=48, high=79, start=start, bass=None, result=r))
        for bs in [dict(low=36, high=55, start=None, inversions=True), dict(low=40, high=60, start=45, inversions=True),
                   dict(low=28, high=52, start=None, inversions=False), dict(low=43, high=50, start=50, inversions=True)]:
            r = attempt(lambda: score.bass_line(prog, **bs))
            ref["bass_line"].append(dict(name=name, chords=prog, **bs, result=r))

    # forced cases: narrow ranges over a bass line, where parallels and "no voicing above the bass" happen
    wrng = random.Random(5)
    wpool = [r + q for r in ["C", "D", "E", "F", "G", "A", "B", "Bb"] for q in ["", "m", "7", "m7", "sus", "5"]]
    forced = 0
    while forced < 25:
        prog = [wrng.choice(wpool) for _ in range(5)]
        for s in [dict(voices=3, low=60, high=72), dict(voices=2, low=55, high=67)]:
            try:
                b = score.bass_line(prog, low=40, high=52)
                vl = score.voice_lead(prog, bass=b, **s)
            except ValueError:
                continue
            if vl.warnings:
                forced += 1
                ref["voice_lead"].append(dict(name=f"forced{forced}", chords=prog, **s, start=None, bass=b,
                                              result={"pitches": [list(v) for v in vl.pitches], "warnings": vl.warnings}))
    high_bass = [70, 72, 69, 71]
    vl = score.voice_lead(["C", "F", "G", "C"], voices=3, low=48, high=72, bass=high_bass)
    ref["voice_lead"].append(dict(name="bass_too_high", chords=["C", "F", "G", "C"], voices=3, low=48, high=72, start=None,
                                  bass=high_bass, result={"pitches": [list(v) for v in vl.pitches], "warnings": vl.warnings}))

    # ---- phrase and swell
    lines = []
    score.setup(bpb=3, unit=0.5, pause=0)
    lines.append(("6/8", score.melody("B4:3 E5:3 | D5:2 C5:1 B4:2 A4:1 | G4:6 | r:2 E4:1 F#4:1 G4:1 A4:1 | B4:6", 9, 76)))
    lines.append(("6/8", score.melody("G4:1 A4:1 B4:1 D5:3 | E5:2 D5:1 B4:3 | A4:3 r:3 | G4:6", 1, 70, legato=0.95)))
    score.setup(bpb=4, unit=1, pause=0)
    lines.append(("4/4", score.melody("G5:1 F#5:1 A5:2 | E5:2 D#5:1 E5:1 | C5:4 | r:1 B4:1 C5:1 D5:1 | A4:4", 25, 100)))
    lines.append(("4/4", score.melody("C4:0.5 D4:0.5 E4:0.5 F4:0.5 G4:2 | A4:1 r:1 B4:1 C5:1 | D5:3 r:1", 3, 88, arc=6)))
    score.setup(bpb=3, unit=1, pause=0)
    lines.append(("3/4", score.melody("G4:2 B4:1 | D5:3 | C5:1 B4:1 A4:1 | G4:3", 1, 80)))
    # chords in a line (two notes at the same beat) and a long note
    lines.append(("4/4", [{"beat": 0, "dur": 1, "pitch": 60, "vel": 80}, {"beat": 0, "dur": 1, "pitch": 64, "vel": 82},
                          {"beat": 1, "dur": 6, "pitch": 67, "vel": 90}, {"beat": 7, "dur": 0.5, "pitch": 65, "vel": 70},
                          {"beat": 7.5, "dur": 0.5, "pitch": 64, "vel": 70}, {"beat": 8, "dur": 2, "pitch": 62, "vel": 75}]))
    opts = [dict(vary=0), dict(vary=0, rit=0), dict(vary=0, breath=0.25), dict(vary=0, long=1.5, rest=1.0),
            dict(vary=0, breaks=(3, 11, 27)), dict(vary=0, accent=10, arc=0, soften=0, rit=0.15),
            dict(vary=0, accent=0, arc=12, soften=4, breath=0.5), dict(vary=2, seed=1), dict(vary=5, seed=7, rit=0.1)]
    ref["phrase"] = []
    for meter, line in lines:
        for o in opts:
            ref["phrase"].append(dict(meter=meter, notes=line, opts=dict(o, breaks=list(o.get("breaks", ()))),
                                      result=score.phrase(line, meter, **o)))
    ref["swell"] = []
    for meter, line in lines:
        for o in [dict(lo=40, hi=100, long=1.5, step=0.25, rest=1.0), dict(lo=20, hi=127, long=3, step=0.5, rest=0.5),
                  dict(lo=60, hi=90, long=1, step=0.125, rest=2.0)]:
            ref["swell"].append(dict(notes=line, opts=o, result=score.swell_cc(line, **o)))

    # ---- ramp and curve
    ref["ramp"] = [dict(args=a, result=score.ramp(*a)) for a in [
        (0, 0, 1, 100, 0.5), (0, 0, 0.75, 1, 0.5), (0, 0, 1.25, 1, 0.5), (2, 10, 1, 20, 0.5), (3.3, 64, 7.1, 20, 0.25),
        (0, 127, 12, 0, 0.5), (1.1, 0.5, 1.6, 3.5, 0.1), (5, 64.5, 6, 65.5, 0.5), (-1, 0, 1, 10, 0.5)]]
    ref["curve"] = []
    for bpb, unit, pts in [(3, 0.5, [(1, 0, 30), (2, 3, 90), (4, 0, 50)]), (4, 1, [(5, 0, 0), (5, 2, 127), (6, 0, 64), (8, 1, 10)]),
                           (4, 1, [(1, 0, 64)]), (3, 0.5, [(9, 0, 40), (9, 1, 41), (12, 5, 100)])]:
        score.setup(bpb=bpb, unit=unit, pause=0)
        ref["curve"].append(dict(points=[[score.at(b, x), v] for b, x, v in pts], result=score.curve(*pts)))

    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(ref, f, indent=None, separators=(",", ":"))
    print(f"wrote {out_path}: " + ", ".join(f"{k} {len(v)}" for k, v in ref.items()))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
