"""Print the mapped key range, keycenters and velocity splits of SFZ files (flat files only).

Usage: python -I tools/sfz_ranges.py file1.sfz [...]
"""
import re
import sys

NOTE = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]


def name(n):
    return f"{NOTE[n % 12]}{n // 12 - 1}"


for p in sys.argv[1:]:
    txt = re.sub(r"//[^\n]*", "", open(p, encoding="utf-8", errors="replace").read())
    lo = [int(x) for x in re.findall(r"\blokey=(\d+)", txt)]
    hi = [int(x) for x in re.findall(r"\bhikey=(\d+)", txt)]
    kc = sorted({int(x) for x in re.findall(r"\bpitch_keycenter=(\d+)", txt)})
    hv = sorted({int(x) for x in re.findall(r"\bhivel=(\d+)", txt)})
    if lo and hi:
        print(f"{p.split('/')[-1]:30s} {name(min(lo))}({min(lo)})..{name(max(hi))}({max(hi)})  "
              f"centers={[name(k) for k in kc][:14]}  vel_splits={hv}")
    else:
        print(p, "no lokey/hikey found")
