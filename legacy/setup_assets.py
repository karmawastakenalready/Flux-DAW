"""setup_assets.py - fetch the renderers and sample libraries from their official sources.

    python setup_assets.py            # asks before any download over 500 MB
    python setup_assets.py --yes      # no prompts

Downloads (skipped if already present):
  tools/sfizz/        sfizz 1.2.3 win64 zip (8 MB)            github.com/sfztools/sfizz releases
  tools/fluidsynth/   FluidSynth 2.6.1 win10-x64 zip (3 MB)   github.com/FluidSynth/fluidsynth releases
  instruments/GeneralUser-GS/        SoundFont (32 MB)              github.com/mrbumpy409/GeneralUser-GS
  instruments/VSCO-2-CE/             sparse subset (~1.4 GB) + SFZ maps   github.com/sgossner/VSCO-2-CE
  instruments/cithara-barbarica/     finger articulation (~77 MB)   github.com/sfzinstruments/cithara-barbarica
  instruments/SalamanderGrandPiano/  full (748 MB, asks first)      github.com/sfzinstruments/SalamanderGrandPiano
On macOS/Linux install sfizz + fluidsynth with your package manager instead (brew/apt) and point
SFIZZ_LIB at libsfizz if it is not on the default library path.
"""
import argparse
import os
import subprocess
import sys
import urllib.request
import zipfile

ROOT = os.path.dirname(os.path.abspath(__file__))
INST = os.path.join(ROOT, "instruments")

VSCO_PATHS = ["/LICENSE", "/README.md", "/Readme.txt", "/Strings/Harp/", "/Strings/Cello Section/susvib/",
              "/Strings/Viola Section/susvib/", "/Strings/Violin Section/susVib/", "/Strings/Solo Contrabass/SusNV/",
              "/Woodwinds/Flute/susvib/", "/Brass/F Horn/sus/", "/Percussion/Timpani/", "/Percussion/TB_hit_*",
              "/Percussion/Tamb*-Hit_*", "/Percussion/BDrumNewhit_*", "/Percussion/Snare*-taps_*",
              "/Percussion/gongHit_*",
              # added for Temporal Shift
              "/Strings/Solo Violin/", "/Strings/Violin Section/Spic/", "/Strings/Violin Section/Pizz/",
              "/Strings/Violin Section/Trem/", "/Strings/Viola Section/spic/", "/Strings/Viola Section/pizz/",
              "/Strings/Viola Section/trem/", "/Strings/Cello Section/spic/", "/Strings/Cello Section/pizzT/",
              "/Strings/Solo Contrabass/Pizz/", "/Strings/Solo Contrabass/Spic/", "/Brass/Trumpet/",
              "/Brass/Tenor Trombone/", "/Brass/F Horn/stac/", "/Keys/Organ/", "/Percussion/", "/Woodwinds/Flute/stac/",
              "/VSCO 1 Percussion/varWood/"]
VSCO_SFZ = ["Harp", "CelloEnsSusVib", "CelloEnsSusVib-Quiet", "ViolaEnsSusVib", "ViolaEnsSusVib-Quiet",
            "ViolinEnsSusVib", "ViolinEnsSusVib-Quiet", "ContrabassSusNV", "FluteSusVib", "FHornSus", "Timpani",
            "TimpaniRolls", "TubularBells",
            # added for Temporal Shift
            "SViolinVib", "SViolinSpic", "SViolinPizz", "ViolinEnsSpic", "ViolinEnsPizz", "ViolinEnsTrem", "ViolaEnsSpic",
            "ViolaEnsPizz", "ViolaEnsTrem", "CelloEnsSpic", "CelloEnsPizz", "ContrabassPizz", "ContrabassSpic",
            "TrumpetSus", "TrumpetStac", "TromboneSus", "TromboneStac", "FHornStac", "OrganLoud", "OrganQuiet",
            "OrganLoudPedal", "OrganQuietPedal", "Glockenspiel", "FluteStac", "GM-StylePerc"]


def git(*args, cwd=None):
    subprocess.run(["git", *args], cwd=cwd, check=True)


def sparse_clone(url, dest, paths, branch=None):
    if os.path.isdir(dest):
        print(f"  exists: {dest}")
        return
    cmd = ["clone", "--depth", "1", "--filter=blob:none", "--no-checkout", "--single-branch"]
    if branch:
        cmd += ["--branch", branch]
    git(*cmd, url, dest)
    subprocess.run(["git", "sparse-checkout", "set", "--no-cone", "--stdin"], cwd=dest, check=True,
                   input="\n".join(paths), text=True)
    git("checkout", cwd=dest)


def fetch(url, path):
    if os.path.exists(path):
        print(f"  exists: {path}")
        return
    os.makedirs(os.path.dirname(path), exist_ok=True)
    print(f"  GET {url}")
    urllib.request.urlretrieve(url, path)


def confirm(msg, yes):
    if yes:
        return True
    return input(f"{msg} [y/N] ").strip().lower() == "y"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--yes", action="store_true")
    a = ap.parse_args()

    if sys.platform == "win32":
        print("tools")
        for url, zpath, dest in (
            ("https://github.com/sfztools/sfizz/releases/download/1.2.3/sfizz-1.2.3-win64.zip",
             os.path.join(ROOT, "tools", "_dl", "sfizz.zip"), os.path.join(ROOT, "tools", "sfizz")),
            ("https://github.com/FluidSynth/fluidsynth/releases/download/v2.6.1/fluidsynth-v2.6.1-win10-x64-cpp11.zip",
             os.path.join(ROOT, "tools", "_dl", "fluidsynth.zip"), os.path.join(ROOT, "tools", "fluidsynth")),
        ):
            fetch(url, zpath)
            if not os.path.isdir(dest):
                zipfile.ZipFile(zpath).extractall(dest)

    print("instruments")
    gu = os.path.join(INST, "GeneralUser-GS")
    base = "https://raw.githubusercontent.com/mrbumpy409/GeneralUser-GS/main"
    fetch(f"{base}/GeneralUser-GS.sf2", os.path.join(gu, "GeneralUser-GS.sf2"))
    fetch(f"{base}/documentation/LICENSE.txt", os.path.join(gu, "LICENSE.txt"))

    vsco = os.path.join(INST, "VSCO-2-CE")
    if os.path.isdir(vsco):   # existing checkout: make sure newly needed folders are included
        subprocess.run(["git", "sparse-checkout", "add", "--stdin"], cwd=vsco, check=True,
                       input="\n".join(VSCO_PATHS), text=True)
    sparse_clone("https://github.com/sgossner/VSCO-2-CE", vsco, VSCO_PATHS)
    for name in VSCO_SFZ:
        fetch(f"https://raw.githubusercontent.com/sgossner/VSCO-2-CE/SFZ/{name}.sfz", os.path.join(vsco, f"{name}.sfz"))

    sparse_clone("https://github.com/sfzinstruments/cithara-barbarica", os.path.join(INST, "cithara-barbarica"),
                 ["/LICENSE", "/readme.txt", "/Programs/", "/Samples/finger/"])

    sal = os.path.join(INST, "SalamanderGrandPiano")
    if os.path.isdir(sal):
        print(f"  exists: {sal}")
    elif confirm("Salamander Grand Piano V3: full repo, 748.5 MB, CC BY 3.0, from "
                 "github.com/sfzinstruments/SalamanderGrandPiano. Download?", a.yes):
        git("clone", "--depth", "1", "--single-branch", "https://github.com/sfzinstruments/SalamanderGrandPiano", sal)
    else:
        print("  skipped Salamander - the piano part will fall back to GeneralUser GS")
    print("done. Next:  python render.py --test")


if __name__ == "__main__":
    main()
