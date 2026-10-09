"""Baton FLAC bridge: convert a 16-bit WAV to 16-bit FLAC (same rate, same samples).

    python -I to_flac.py in.wav out.flac
"""
import sys

import soundfile as sf


def main(src, dst):
    data, sr = sf.read(src, dtype="int16")
    sf.write(dst, data, sr, subtype="PCM_16", format="FLAC")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
