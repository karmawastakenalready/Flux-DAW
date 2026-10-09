"""Export a pipeline song's mix (pieces/<piece>.py MIX / REVERB and master.py MASTER) as mix.json beside its
meta file, so `flux open` / `flux import` bring the song in with its mix.

    python daw/bridge/export_mix.py output/<title>/<title>.json [more meta files...]
"""
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, ROOT)

import master  # noqa: E402
from compose import load_piece  # noqa: E402


def export(meta_path):
    meta = json.load(open(meta_path))
    mod = load_piece(meta["piece"])
    out = dict(
        piece=meta["piece"],
        version=meta.get("version"),
        tracks=mod.mix_for(meta.get("version")),
        reverb=getattr(mod, "REVERB", master.REVERB),
        master_eq=master.MASTER["eq"],
        glue=master.MASTER["glue"],
        target_lufs=master.MASTER["target_lufs"],
        ceiling_dbtp=master.MASTER["ceiling_dbtp"],
    )
    path = os.path.join(os.path.dirname(os.path.abspath(meta_path)), "mix.json")
    with open(path, "w") as f:
        json.dump(out, f, indent=1)
    print(f"wrote {path} ({len(out['tracks'])} tracks)")


if __name__ == "__main__":
    for p in sys.argv[1:]:
        export(p)
