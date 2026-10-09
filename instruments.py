"""Instrument registry: where each part's samples live, which engine plays it, and its playable range.

Every melodic/harmonic part comes from a real sample library (see LICENSES.md). Each piece uses an
instrument SET (one MIDI track per instrument). `prepare_sfz(set)` writes wrapper SFZ files into
build/sfz/<set>/ that point at the untouched downloaded libraries and add controller handling:
  * CC1  -> crossfade between the recorded dynamic layers + a gentle brightness filter (strings)
  * CC11 -> expression (sfizz handles CC7/CC11 natively)
  * keyswitch notes 120+ -> articulation changes inside one instrument track (sus/trem/spic/pizz/stac)
"""
import os
import re

ROOT = os.path.dirname(os.path.abspath(__file__))
INST_DIR = os.path.join(ROOT, "instruments")
BUILD_SFZ = os.path.join(ROOT, "build", "sfz")
GENERALUSER_SF2 = os.path.join(INST_DIR, "GeneralUser-GS", "GeneralUser-GS.sf2")

VSCO = os.path.join(INST_DIR, "VSCO-2-CE")

# range = (lowest, highest) MIDI note we allow the composer to write (realistic for the real
# instrument AND covered by the sample map). gm = (bank, program) for the GeneralUser GS fallback.
CAPITAL_CITY = {
    "flute":      dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "FluteSusVib.sfz"), wrap="expr",
                       range=(62, 93), test_note=74, max_note_s=4.4, gm=(0, 73), channel=0),
    "horn":       dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "FHornSus.sfz"), wrap="expr",
                       range=(38, 77), test_note=62, max_note_s=3.8, gm=(0, 60), channel=1),
    "lyre":       dict(lib="cithara-barbarica",
                       src=os.path.join(INST_DIR, "cithara-barbarica", "Programs", "02-cithara_barbarica_finger.sfz"),
                       wrap=None, range=(52, 72), test_note=62, max_note_s=None, gm=(0, 24), channel=2),
    "harp":       dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "Harp.sfz"), wrap="expr",
                       range=(28, 101), test_note=62, max_note_s=None, gm=(0, 46), channel=3,
                       extra_global=[("ampeg_release", "3.5")]),  # harpist lets notes ring, not forever
    "violins":    dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "ViolinEnsSusVib.sfz"), wrap="strings",
                       range=(55, 86), test_note=74, max_note_s=8.0, gm=(0, 49), channel=4),
    "violas":     dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "ViolaEnsSusVib.sfz"), wrap="strings",
                       range=(48, 81), test_note=62, max_note_s=7.0, gm=(0, 49), channel=5),
    "cellos":     dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "CelloEnsSusVib.sfz"), wrap="strings",
                       range=(36, 72), test_note=50, max_note_s=6.0, gm=(0, 48), channel=6),
    # D1 needs a low-C extension, standard on orchestral basses
    "basses":     dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "ContrabassSusNV.sfz"), wrap="strings",
                       range=(26, 55), test_note=38, max_note_s=6.0, gm=(0, 43), channel=7),
    "piano":      dict(lib="SalamanderGrandPiano",
                       src=os.path.join(INST_DIR, "SalamanderGrandPiano", "Salamander Grand Piano V3.sfz"),
                       wrap=None, range=(21, 108), test_note=50, max_note_s=None, gm=(0, 0), channel=8),
    "percussion": dict(lib="VSCO-2-CE", src=os.path.join(ROOT, "sfz", "medieval_perc.sfz"), wrap=None,
                       range=(36, 54), test_note=36, test_notes=(36, 38, 52, 54), max_note_s=None,
                       gm=(128, 48), channel=9),
    "timpani":    dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "Timpani.sfz"), wrap="expr",
                       range=(38, 50), test_note=38, max_note_s=None, gm=(0, 47), channel=10,
                       extra_global=[("ampeg_release", "3")]),
    "bells":      dict(lib="VSCO-2-CE", src=os.path.join(VSCO, "TubularBells.sfz"), wrap="expr",
                       range=(60, 79), test_note=74, max_note_s=None, gm=(0, 14), channel=11,
                       extra_global=[("ampeg_release", "7")]),
}



def _v(name):
    return os.path.join(VSCO, name)


GM_PERC = _v("GM-StylePerc.sfz")
SALAMANDER = os.path.join(INST_DIR, "SalamanderGrandPiano", "Salamander Grand Piano V3.sfz")
KS_BASE = 120   # keyswitch notes: 120 = first articulation listed (default), 121 = second, ...

# arts = [(articulation, source sfz, "xfade" for CC1 dynamic-layer crossfade | None), ...]
TEMPORAL_SHIFT = {
    "violin_solo": dict(lib="VSCO-2-CE", arts=[("sus", _v("SViolinVib.sfz"), "xfade"), ("spic", _v("SViolinSpic.sfz"), None),
                                               ("pizz", _v("SViolinPizz.sfz"), None)],
                        range=(55, 93), test_note=76, max_note_s=10.0, gm=(0, 40)),
    "violins":     dict(lib="VSCO-2-CE", arts=[("sus", _v("ViolinEnsSusVib.sfz"), "xfade"), ("trem", _v("ViolinEnsTrem.sfz"), "xfade"),
                                               ("spic", _v("ViolinEnsSpic.sfz"), None), ("pizz", _v("ViolinEnsPizz.sfz"), None)],
                        range=(55, 86), test_note=74, max_note_s=7.0, gm=(0, 48)),
    "violas":      dict(lib="VSCO-2-CE", arts=[("sus", _v("ViolaEnsSusVib.sfz"), "xfade"), ("trem", _v("ViolaEnsTrem.sfz"), "xfade"),
                                               ("spic", _v("ViolaEnsSpic.sfz"), None), ("pizz", _v("ViolaEnsPizz.sfz"), None)],
                        range=(48, 81), test_note=62, max_note_s=7.0, gm=(0, 48)),
    "cellos":      dict(lib="VSCO-2-CE", arts=[("sus", _v("CelloEnsSusVib.sfz"), "xfade"), ("spic", _v("CelloEnsSpic.sfz"), None),
                                               ("pizz", _v("CelloEnsPizz.sfz"), None)],
                        range=(36, 72), test_note=50, max_note_s=6.0, gm=(0, 48)),
    "basses":      dict(lib="VSCO-2-CE", arts=[("sus", _v("ContrabassSusNV.sfz"), "xfade"), ("spic", _v("ContrabassSpic.sfz"), None),
                                               ("pizz", _v("ContrabassPizz.sfz"), None)],
                        range=(28, 55), test_note=40, max_note_s=6.0, gm=(0, 43)),
    "trumpet":     dict(lib="VSCO-2-CE", arts=[("sus", _v("TrumpetSus.sfz"), None), ("stac", _v("TrumpetStac.sfz"), None)],
                        range=(54, 82), test_note=71, max_note_s=5.0, gm=(0, 56)),
    "trombone":    dict(lib="VSCO-2-CE", arts=[("sus", _v("TromboneSus.sfz"), None), ("stac", _v("TromboneStac.sfz"), None)],
                        range=(34, 65), test_note=47, max_note_s=4.0, gm=(0, 57)),
    "horn":        dict(lib="VSCO-2-CE", arts=[("sus", _v("FHornSus.sfz"), None), ("stac", _v("FHornStac.sfz"), None)],
                        range=(38, 77), test_note=59, max_note_s=3.8, gm=(0, 60)),
    "flute":       dict(lib="VSCO-2-CE", arts=[("sus", _v("FluteSusVib.sfz"), None), ("stac", _v("FluteStac.sfz"), None)],
                        range=(62, 93), test_note=78, max_note_s=4.4, gm=(0, 73)),
    "organ":       dict(lib="VSCO-2-CE", arts=[("quiet", _v("OrganQuiet.sfz"), None), ("loud", _v("OrganLoud.sfz"), None)],
                        range=(43, 86), test_note=59, max_note_s=11.0, gm=(0, 19)),
    "organ_pedal": dict(lib="VSCO-2-CE", arts=[("quiet", _v("OrganQuietPedal.sfz"), None), ("loud", _v("OrganLoudPedal.sfz"), None)],
                        range=(36, 55), test_note=47, max_note_s=11.0, gm=(0, 19)),
    "piano":       dict(lib="SalamanderGrandPiano", src=SALAMANDER, wrap=None,
                        range=(21, 108), test_note=59, max_note_s=None, gm=(0, 0)),
    "harp":        dict(lib="VSCO-2-CE", src=_v("Harp.sfz"), wrap="expr", range=(28, 101), test_note=59,
                        max_note_s=None, gm=(0, 46), extra_global=[("ampeg_release", "3.5")]),
    "glock":       dict(lib="VSCO-2-CE", src=_v("Glockenspiel.sfz"), wrap="expr", range=(67, 96), test_note=78,
                        max_note_s=None, gm=(0, 9), extra_global=[("ampeg_release", "4")]),
    "bells":       dict(lib="VSCO-2-CE", src=_v("TubularBells.sfz"), wrap="expr", range=(60, 79), test_note=71,
                        max_note_s=None, gm=(0, 14), extra_global=[("ampeg_release", "7")]),
    "timpani":     dict(lib="VSCO-2-CE", src=_v("Timpani.sfz"), wrap="expr", range=(38, 50), test_note=40,
                        max_note_s=None, gm=(0, 47), extra_global=[("ampeg_release", "3")]),
    # GM-style kit: 36 bass drum, 37 snare taps, 38 snare, 39 snare roll, 46 gong, 47/48/50 sus-cymbal swells,
    # 49 crash, 51 sus cymbal, 54 tambourine, 69-71 ratchet, 75 claves, 76/77 log drum hi/lo,
    # 78-81 triangle, 83-86 bell tree, 101-104 bowed cymbal
    "percussion":  dict(lib="VSCO-2-CE", src=GM_PERC, wrap="expr", range=(32, 104), test_note=38,
                        test_notes=(36, 38, 49, 51, 101), max_note_s=None, gm=(128, 48)),
    "clock":       dict(lib="VSCO-2-CE", src=GM_PERC, wrap="expr", range=(69, 77), test_note=76,
                        test_notes=(69, 75, 76, 77), max_note_s=None, gm=(128, 48)),
    # reversed one-shots: render.py renders each note alone, reverses it and lands its attack on the note END
    "rev_cymbal":  dict(lib="VSCO-2-CE", src=GM_PERC, wrap="expr", reverse=True, range=(49, 51), test_note=51,
                        max_note_s=None, gm=(128, 48)),
    "rev_piano":   dict(lib="SalamanderGrandPiano", src=SALAMANDER, wrap=None, reverse=True, range=(21, 108),
                        test_note=59, max_note_s=None, gm=(0, 0)),
}

# Karoryfer Caveman Cosmonaut (CC0): a sampled transistor organ played as a virtual-analog synth.
# sfizz needs two fixes, applied to a copy in build/: the ARIA alias loop_mode=continuous, and the ARIA-only
# var0N_* cutoff modulation (replaced by a standard cutoff_oncc120 so CC120 really opens the filter).
CAVEMAN = os.path.join(INST_DIR, "karoryfer.caveman-cosmonaut", "Programs", "main.sfz")
CAVEMAN_PATCH = dict(
    replace=[("loop_mode=continuous", "loop_mode=loop_continuous"),
             ("cutoff=250\n", "cutoff=250\ncutoff_oncc120=6000\nfil_veltrack=2400\n")],
    regex=[(r"var0[12]_[^\n]*\n", "")],
)

SFZ_DIR = os.path.join(ROOT, "sfz")

GUITAR = os.path.join(INST_DIR, "karoryfer.black-and-green-guitars", "Programs", "02-black_keyswitch.sfz")

# Makima boss fight: sampled synths + electronic kit + guitar, with piano, music box and orchestral layers.
MAKIMA = {
    "drums":       dict(lib="sonic-pi", src=os.path.join(SFZ_DIR, "makima_kit.sfz"), wrap=None, range=(34, 58),
                        test_note=36, test_notes=(36, 38, 42, 46, 49), max_note_s=None, gm=(128, 25)),
    "sub808":      dict(lib="sonic-pi", src=os.path.join(SFZ_DIR, "sonicpi_808.sfz"), wrap=None, range=(26, 40),
                        test_note=28, max_note_s=None, gm=(0, 38)),
    "synth_bass":  dict(lib="karoryfer.caveman-cosmonaut", src=CAVEMAN, patch=CAVEMAN_PATCH, range=(28, 52),
                        test_note=40, max_note_s=None, gm=(0, 38)),
    "synth_lead":  dict(lib="karoryfer.caveman-cosmonaut", src=CAVEMAN, patch=CAVEMAN_PATCH, range=(52, 88),
                        test_note=71, max_note_s=None, gm=(0, 81)),
    "synth_pad":   dict(lib="karoryfer.caveman-cosmonaut", src=CAVEMAN, patch=CAVEMAN_PATCH, range=(40, 84),
                        test_note=59, max_note_s=None, gm=(0, 89)),
    "eerie_pad":   dict(lib="karoryfer.cowsynth", src=os.path.join(INST_DIR, "karoryfer.cowsynth", "cowsynth_shifty_pad.sfz"),
                        wrap=None, range=(40, 79), test_note=59, max_note_s=None, gm=(0, 94)),
    "guitar":      dict(lib="karoryfer.black-and-green-guitars", src=GUITAR, wrap=None, range=(40, 76),
                        ks_map={"ord": 36, "stac": 37, "btb": 38}, test_note=40, max_note_s=None, gm=(0, 30)),
    # second take of the same riff (independently humanised) for a hard-panned double-tracked wall
    "guitar_dbl":  dict(lib="karoryfer.black-and-green-guitars", src=GUITAR, wrap=None, range=(40, 76),
                        ks_map={"ord": 36, "stac": 37, "btb": 38}, test_note=40, max_note_s=None, gm=(0, 30)),
    "piano":       dict(lib="SalamanderGrandPiano", src=SALAMANDER, wrap=None, range=(21, 108), test_note=64,
                        max_note_s=None, gm=(0, 0)),
    "glock":       TEMPORAL_SHIFT["glock"],
    "violins":     TEMPORAL_SHIFT["violins"],
    "cellos":      TEMPORAL_SHIFT["cellos"],
    "trombone":    TEMPORAL_SHIFT["trombone"],
    "horn":        TEMPORAL_SHIFT["horn"],
    "timpani":     TEMPORAL_SHIFT["timpani"],
    "chains":      dict(TEMPORAL_SHIFT["percussion"], test_notes=(67, 68, 69, 82, 101)),
    "fx":          dict(lib="sonic-pi", src=os.path.join(SFZ_DIR, "makima_fx.sfz"), wrap=None, range=(28, 51),
                        test_note=28, test_notes=(28, 29, 33, 40, 48), max_note_s=None, gm=(128, 56)),
    "rev_fx":      dict(lib="sonic-pi", src=os.path.join(SFZ_DIR, "makima_fx.sfz"), wrap=None, reverse=True,
                        range=(28, 51), test_note=28, max_note_s=None, gm=(128, 56)),
    "rev_cymbal":  TEMPORAL_SHIFT["rev_cymbal"],
}

# Far Roads (open-world exploration, built in Flux by daw/projects/far_roads_build.py). Ranges are MIDI keys.
# The folk harp (sfz/far_roads_folk_harp.sfz) and the tenor recorder are VCSL maps loaded directly.
FAR_ROADS = {
    "flute":     dict(lib="VSCO-2-CE", src=_v("FluteSusNV.sfz"), wrap="strings", range=(60, 93)),
    "flute_vib": dict(lib="VSCO-2-CE", src=_v("FluteExpVib.sfz"), wrap="strings", range=(60, 93)),
    "oboe":      dict(lib="VSCO-2-CE", src=_v("OboeSusVib.sfz"), wrap="strings", range=(58, 86)),
    "clarinet":  dict(lib="VSCO-2-CE", src=_v("ClarinetSus.sfz"), wrap="strings", range=(50, 86)),
    "violins":   dict(lib="VSCO-2-CE", src=_v("ViolinEnsSusVib.sfz"), wrap="strings", range=(55, 86)),
    "violas":    dict(lib="VSCO-2-CE", src=_v("ViolaEnsSusVib.sfz"), wrap="strings", range=(48, 81)),
    "cellos":    dict(lib="VSCO-2-CE", src=_v("CelloEnsSusVib.sfz"), wrap="strings", range=(36, 72)),
    "basses":    dict(lib="VSCO-2-CE", src=_v("ContrabassSusNV.sfz"), wrap="strings", range=(28, 55)),
    "harp":      dict(lib="VSCO-2-CE", src=_v("Harp.sfz"), wrap="expr", range=(28, 101),
                      extra_global=[("ampeg_release", "3.5")]),
    "horn":      dict(lib="VSCO-2-CE", src=_v("FHornSus.sfz"), wrap="expr", range=(38, 77)),
    "timpani":   dict(lib="VSCO-2-CE", src=_v("Timpani.sfz"), wrap="expr", range=(38, 50),
                      extra_global=[("ampeg_release", "3")]),
}

INSTRUMENT_SETS = {"capital_city": CAPITAL_CITY, "temporal_shift": TEMPORAL_SHIFT, "makima": MAKIMA, "far_roads": FAR_ROADS}
INSTRUMENTS = CAPITAL_CITY          # default set (backwards compatible)
ORDER = list(INSTRUMENTS)


def ks_pitch(spec, art):
    return KS_BASE + [a[0] for a in spec["arts"]].index(art)

# CC1 crossfade window between the soft and loud recorded layers
XFADE_LO, XFADE_HI = 40, 100


def _parse_blocks(text):
    """Split a flat SFZ (one opcode per line, as VSCO's are) into [(header, [(k, v), ...]), ...]."""
    blocks = [("", [])]
    for raw in text.splitlines():
        line = raw.split("//", 1)[0].strip()
        if not line:
            continue
        m = re.match(r"^<(\w+)>\s*(.*)$", line)
        if m:
            blocks.append((m.group(1), []))
            line = m.group(2).strip()
            if not line:
                continue
        if "=" in line:
            if line.startswith(("default_path=", "sample=")):
                k, v = line.split("=", 1)
                blocks[-1][1].append((k.strip(), v.strip()))
            else:
                for tok in re.findall(r"(\w+)=(\S+)", line):
                    blocks[-1][1].append(tok)
    return blocks


def _emit(blocks):
    out = []
    for header, ops in blocks:
        if header:
            out.append(f"<{header}>")
        out.extend(f"{k}={v}" for k, v in ops)
        out.append("")
    return "\n".join(out)


_DYN_LETTERS = {"pp": 1, "p": 2, "mp": 3, "mf": 4, "f": 5, "ff": 6}


def _layer(sample):
    """Recorded dynamic layer from a VSCO sample name: '..._v3_..' -> 3; '..._p.wav' / '..._f_..' -> 2 / 5."""
    m = re.search(r"_v(\d)", sample)
    if m:
        return int(m.group(1))
    m = re.search(r"_(pp|p|mp|mf|f|ff)(?:_|\.)", sample)
    return _DYN_LETTERS[m.group(1)] if m else None


def _wrap(name, spec):
    return _emit(_wrap_blocks(name, spec))


def _wrap_blocks(name, spec):
    src = spec["src"]
    text = open(src, encoding="utf-8", errors="replace").read()
    blocks = _parse_blocks(text)
    src_dir = os.path.dirname(src)
    dp = next((v for _, ops in blocks for k, v in ops if k == "default_path"), "")
    new = []
    for header, ops in blocks:
        ops = list(ops)
        if header == "control":
            ops = [(k, v) for k, v in ops if k != "default_path"]
            abs_dp = os.path.join(src_dir, dp.replace("\\", "/")).replace("\\", "/")
            if not abs_dp.endswith("/"):
                abs_dp += "/"
            ops.insert(0, ("default_path", abs_dp))
            ops.append(("set_cc1", "64"))
            ops.append(("set_cc11", "127"))
        if header == "global":
            if spec["wrap"] == "strings":
                ops += [("xf_cccurve", "power"), ("amp_veltrack", "45"),
                        ("fil_type", "lpf_2p"), ("cutoff", "4500"), ("cutoff_oncc1", "2000"), ("volume_oncc1", "6"),
                        ("ampeg_attack", "0.06"), ("ampeg_release", "0.5")]
            ops += list(spec.get("extra_global", []))
        new.append((header, ops))

    if spec["wrap"] == "strings":
        # find recorded dynamic layers from sample names (..._v1_..., ..._v3_...)
        layers = set()
        for header, ops in new:
            if header == "region":
                lay = _layer(dict(ops).get("sample", ""))
                if lay is not None:
                    layers.add(lay)
        soft, loud = min(layers), max(layers)
        seen = set()
        out = []
        for header, ops in new:
            if header != "region":
                out.append((header, ops))
                continue
            d = dict(ops)
            layer = _layer(d.get("sample", ""))
            layer = soft if layer is None else layer
            if layer not in (soft, loud):
                continue
            key = (layer, d.get("lokey"), d.get("hikey"))
            if key in seen:  # alternate take of the same note/layer: keep one, avoid doubling
                continue
            seen.add(key)
            ops = [(k, v) for k, v in ops if k not in ("lovel", "hivel")]
            if layer == soft:
                ops += [("xfout_locc1", str(XFADE_LO)), ("xfout_hicc1", str(XFADE_HI))]
            else:
                ops += [("xfin_locc1", str(XFADE_LO)), ("xfin_hicc1", str(XFADE_HI))]
            out.append((header, ops))
        new = out
    return new


def _wrap_multi(name, spec):
    """One instrument, several articulations selected by keyswitch notes KS_BASE+i (first = default).
    Each source's <global> becomes a keyswitched <master>; each keeps its own <control> default_path."""
    out = []
    for i, art in enumerate(spec["arts"]):
        aname, src, mode = art[0], art[1], art[2]
        sub = dict(src=src, wrap="strings" if mode == "xfade" else "expr", extra_global=spec.get("extra_global", []))
        sw = [("sw_lokey", str(KS_BASE)), ("sw_hikey", str(KS_BASE + 7)), ("sw_last", str(KS_BASE + i)),
              ("sw_default", str(KS_BASE))]
        had_master = False
        for header, ops in _wrap_blocks(name, sub):
            if header == "global":
                out.append(("master", sw + list(ops)))
                had_master = True
            elif header == "":
                continue
            else:
                if header in ("group", "region") and not had_master:
                    out.append(("master", sw))
                    had_master = True
                out.append((header, ops))
    legend = ", ".join(f"{KS_BASE + i}={a[0]}" for i, a in enumerate(spec["arts"]))
    return f"// generated multi-articulation instrument '{name}' - keyswitches: {legend}\n" + _emit(out)


def _patched_program(name, spec, out_dir):
    """Copy an SFZ program (and the include tree beside it) into build/, pointing default_path back at the
    original folder and applying opcode fixes (e.g. ARIA alias loop_mode=continuous -> loop_continuous)."""
    import shutil
    src = spec["src"]
    src_dir = os.path.dirname(src)
    dst_dir = os.path.join(out_dir, name)
    if os.path.isdir(dst_dir):
        shutil.rmtree(dst_dir)
    shutil.copytree(src_dir, dst_dir, ignore=shutil.ignore_patterns("*.wav", "*.flac", "*.png", "*.xml"))
    dst = os.path.join(dst_dir, os.path.basename(src))
    text = open(dst, encoding="utf-8", errors="replace").read()
    for a, b in spec["patch"].get("replace", []):
        text = text.replace(a, b)
    for pat, repl in spec["patch"].get("regex", []):
        text = re.sub(pat, repl, text)
    abs_dp = src_dir.replace("\\", "/") + "/"
    if "<control>" in text:
        text = text.replace("<control>", f"<control>\ndefault_path={abs_dp}", 1)
    else:
        text = f"<control>\ndefault_path={abs_dp}\n" + text
    open(dst, "w", encoding="utf-8").write(text)
    return dst


def prepare_sfz(set_name="capital_city"):
    """Write wrapper SFZ files for an instrument set; return {name: path_to_load}."""
    out_dir = os.path.join(BUILD_SFZ, set_name)
    os.makedirs(out_dir, exist_ok=True)
    paths = {}
    for name, spec in INSTRUMENT_SETS[set_name].items():
        if "arts" in spec:
            p = os.path.join(out_dir, f"{name}.sfz")
            with open(p, "w", encoding="utf-8") as f:
                f.write(_wrap_multi(name, spec))
            paths[name] = p
        elif "patch" in spec:
            paths[name] = _patched_program(name, spec, out_dir)
        elif spec["wrap"]:
            p = os.path.join(out_dir, f"{name}.sfz")
            with open(p, "w", encoding="utf-8") as f:
                f.write(f"// generated by instruments.py from {os.path.relpath(spec['src'], ROOT)}\n")
                f.write(_wrap(name, spec))
            paths[name] = p
        else:
            paths[name] = spec["src"]
    return paths


if __name__ == "__main__":
    import sys
    for k, v in prepare_sfz(sys.argv[1] if len(sys.argv) > 1 else "capital_city").items():
        print(f"{k:11s} {v}")
