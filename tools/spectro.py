"""Log-frequency spectrogram of a WAV (or a stack of WAVs) as a PNG, no plotting libraries needed.

    python -I tools/spectro.py OUT.png START_S END_S A.wav [B.wav ...]

Each file becomes one band (top to bottom), 30 Hz - 16 kHz, 80 dB range, with a grid line every second.
"""
import struct
import sys
import zlib

import numpy as np
import soundfile as sf

W, H, FMIN, FMAX, RANGE = 1400, 220, 30.0, 16000.0, 80.0


def colour(v):
    """0..1 -> dark brown .. cream, so it reads like the Flux theme"""
    stops = np.array([[24, 18, 14], [92, 52, 34], [184, 110, 60], [236, 196, 120], [252, 246, 228]], float)
    x = np.clip(v, 0, 1) * (len(stops) - 1)
    i = np.minimum(x.astype(int), len(stops) - 2)
    f = (x - i)[..., None]
    return (stops[i] * (1 - f) + stops[i + 1] * f).astype(np.uint8)


def band(path, t0, t1):
    x, sr = sf.read(path, always_2d=True)
    m = x.mean(1)[int(t0 * sr):int(t1 * sr)]
    n = int(__import__("os").environ.get("SPECTRO_N", "4096"))
    hop = max(1, (len(m) - n) // W)
    win = np.hanning(n)
    freqs = np.fft.rfftfreq(n, 1 / sr)
    rows = np.geomspace(FMAX, FMIN, H)
    idx = np.searchsorted(freqs, rows)
    img = np.zeros((H, W))
    for c in range(W):
        seg = m[c * hop:c * hop + n]
        if len(seg) < n:
            seg = np.pad(seg, (0, n - len(seg)))
        spec = 20 * np.log10(np.abs(np.fft.rfft(seg * win)) / (n / 4) + 1e-9)
        img[:, c] = spec[np.clip(idx, 0, len(spec) - 1)]
    v = (img + RANGE) / RANGE
    rgb = colour(v)
    secs = np.arange(np.ceil(t0), t1)
    for s in secs:
        c = int((s - t0) / (t1 - t0) * W)
        if 0 <= c < W:
            rgb[:, c] = (rgb[:, c] * 0.6 + np.array([90, 140, 200]) * 0.4).astype(np.uint8)
    for f in (50, 100, 200, 500, 1000, 2000, 5000, 10000):
        r = int(np.argmin(np.abs(rows - f)))
        rgb[r, ::4] = [90, 140, 200]
    rgb[-2:] = 255
    return rgb


def write_png(path, rgb):
    h, w, _ = rgb.shape
    raw = b"".join(b"\x00" + rgb[y].tobytes() for y in range(h))

    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))


if __name__ == "__main__":
    out, t0, t1, files = sys.argv[1], float(sys.argv[2]), float(sys.argv[3]), sys.argv[4:]
    write_png(out, np.concatenate([band(p, t0, t1) for p in files]))
    print(out)
