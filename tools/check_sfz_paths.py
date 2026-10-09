"""Check that every sample referenced by the given SFZ files exists on disk.

Usage: python -I tools/check_sfz_paths.py file1.sfz [file2.sfz ...]
"""
import os
import re
import sys


def check(sfz_path):
    root = os.path.dirname(os.path.abspath(sfz_path))
    txt = open(sfz_path, encoding="utf-8", errors="replace").read()
    txt = re.sub(r"//[^\n]*", "", txt)
    m = re.search(r"default_path\s*=\s*([^\n]+?)\s*$", txt, flags=re.M)
    base = m.group(1).strip().replace("\\", "/") if m else ""
    samples = [s.strip().replace("\\", "/") for s in re.findall(r"sample\s*=\s*([^\n]+?\.(?:wav|flac))", txt, flags=re.I)]
    missing = [s for s in samples if not os.path.exists(os.path.join(root, base, s))]
    print(f"{os.path.basename(sfz_path):34s} base={base!r:32s} samples={len(samples):4d} missing={len(missing)} {missing[:2]}")


for p in sys.argv[1:]:
    check(p)
