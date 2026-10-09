"""run.py - the whole pipeline in one command.

    python run.py                                        # Capital City
    python run.py --piece temporal_shift                 # Temporal Shift: explore AND battle
    python run.py --piece temporal_shift --version battle --seed 12
    python run.py --skip-test                            # skip the per-instrument test-note gate

Steps: instrument test gate -> compose -> render stems -> mix + master -> verify.
If verification fails on loudness or true peak, the master is re-run with corrected settings
(up to 3 attempts). Any other failure stops with a non-zero exit code.
Outputs land in output/<title>/ (e.g. output/temporal_shift_battle/).
"""
import argparse
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
PY = sys.executable
DEFAULT_VERSIONS = {"capital_city": [None], "temporal_shift": ["explore", "battle"], "makima": [None]}
SETS = {"capital_city": "capital_city", "temporal_shift": "temporal_shift", "makima": "makima"}


def step(title, args):
    print(f"\n=== {title} ===", flush=True)
    return subprocess.run([PY, *args], cwd=ROOT).returncode


TITLES = {"makima": "makima_boss"}


def build_one(piece, version, seed):
    title = TITLES.get(piece, piece) if version is None else f"{piece}_{version}"
    folder = os.path.join(ROOT, "output", title)
    mid = os.path.join(folder, f"{title}.mid")
    meta = os.path.join(folder, f"{title}.json")
    args = ["compose.py", "--piece", piece, "--seed", str(seed), "--out", mid]
    if version:
        args += ["--version", version]
    if step(f"compose {title} (seed {seed})", args):
        sys.exit("compose failed")
    if step(f"render stems {title}", ["render.py", "--midi", mid]):
        sys.exit("render failed")
    target, ceiling = -15.0, -1.0
    for attempt in range(1, 4):
        if step(f"mix + master {title} (attempt {attempt})",
                ["master.py", "--meta", meta, "--target-lufs", str(target), "--ceiling-dbtp", str(ceiling)]):
            sys.exit("master failed")
        if step(f"verify {title}", ["verify.py", "--meta", meta]) == 0:
            m = json.load(open(meta))
            base = os.path.join(folder, title + ("_loop" if m.get("loop") else ""))
            return f"{base}.flac"
        v = json.load(open(os.path.join(folder, title + "_loop_verify.json")))
        fixable = {"integrated_lufs", "true_peak", "clipped_samples"}
        if not set(v["fails"]) <= fixable:
            sys.exit(f"verification failed on {v['fails']} - needs a human look")
        if "true_peak" in v["fails"] or "clipped_samples" in v["fails"]:
            ceiling -= 0.3
        if "integrated_lufs" in v["fails"]:
            lufs = float(v["checks"]["integrated_lufs"]["detail"].split()[0])
            target += -15.0 - lufs
        print(f"  -> retrying master with target {target:.2f} LUFS, ceiling {ceiling:.2f} dBTP")
    sys.exit("verification still failing after 3 master attempts")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--piece", default="capital_city", choices=sorted(DEFAULT_VERSIONS))
    ap.add_argument("--version", default=None, help="e.g. explore / battle (default: all versions of the piece)")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--skip-test", action="store_true")
    a = ap.parse_args()
    if not a.skip_test and step(f"instrument test notes ({SETS[a.piece]})", ["render.py", "--test", "--set", SETS[a.piece]]):
        sys.exit("instrument test gate failed - not composing")
    versions = [a.version] if a.version else DEFAULT_VERSIONS[a.piece]
    done = [build_one(a.piece, v, a.seed) for v in versions]
    print("\nDONE:\n  " + "\n  ".join(done))


if __name__ == "__main__":
    main()
