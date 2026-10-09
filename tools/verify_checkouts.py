"""Byte-exact integrity check of every checked-out file in the instrument git repos.

Git for Windows ships core.autocrlf=true system-wide, which can rewrite bytes in binary files on checkout.
This hashes each working-tree file *without* filters and compares it with the blob id git recorded.

Usage: python -I tools/verify_checkouts.py instruments/<repo> [...]
"""
import hashlib
import os
import subprocess
import sys


def blob_id(path):
    data = open(path, "rb").read()
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


bad_total = 0
for repo in sys.argv[1:]:
    out = subprocess.run(["git", "-C", repo, "ls-files", "-s"], capture_output=True, text=True, check=True).stdout
    bad, n = [], 0
    for line in out.splitlines():
        meta, path = line.split("\t", 1)
        sha = meta.split()[1]
        full = os.path.join(repo, path)
        if not os.path.exists(full):          # outside the sparse-checkout
            continue
        n += 1
        if blob_id(full) != sha:
            bad.append(path)
    bad_total += len(bad)
    print(f"{repo:55s} files={n:5d} corrupted={len(bad)}" + (f"  e.g. {bad[:2]}" if bad else ""))
sys.exit(1 if bad_total else 0)
