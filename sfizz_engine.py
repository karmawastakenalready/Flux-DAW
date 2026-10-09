"""Minimal ctypes binding to the sfizz C API (sfizz.dll / libsfizz) for offline float32 rendering.

sfizz_render.exe only writes 16-bit WAV, so we drive the library directly and keep everything
in float32 until the final master export.
"""
import ctypes
import os
import sys

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
_DEFAULT_LIB = {
    "win32": os.path.join(HERE, "tools", "sfizz", "bin", "Release", "sfizz.dll"),
    "darwin": "libsfizz.dylib",
}.get(sys.platform, "libsfizz.so")

_lib = None


def lib():
    global _lib
    if _lib is None:
        path = os.environ.get("SFIZZ_LIB", _DEFAULT_LIB)
        _lib = ctypes.CDLL(path)
        L = _lib
        L.sfizz_create_synth.restype = ctypes.c_void_p
        L.sfizz_free.argtypes = [ctypes.c_void_p]
        L.sfizz_load_file.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        L.sfizz_load_file.restype = ctypes.c_bool
        L.sfizz_set_sample_rate.argtypes = [ctypes.c_void_p, ctypes.c_float]
        L.sfizz_set_samples_per_block.argtypes = [ctypes.c_void_p, ctypes.c_int]
        L.sfizz_set_num_voices.argtypes = [ctypes.c_void_p, ctypes.c_int]
        L.sfizz_enable_freewheeling.argtypes = [ctypes.c_void_p]
        L.sfizz_get_num_regions.argtypes = [ctypes.c_void_p]
        L.sfizz_get_num_regions.restype = ctypes.c_int
        L.sfizz_get_num_active_voices.argtypes = [ctypes.c_void_p]
        L.sfizz_get_num_active_voices.restype = ctypes.c_int
        L.sfizz_send_note_on.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int]
        L.sfizz_send_note_off.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int]
        L.sfizz_send_cc.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int]
        L.sfizz_send_pitch_wheel.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
        L.sfizz_render_block.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.POINTER(ctypes.c_float)),
                                         ctypes.c_int, ctypes.c_int]
    return _lib


class SfizzSynth:
    def __init__(self, sfz_path, sample_rate=44100, block=256, voices=256):
        L = lib()
        self.block = block
        self.sr = sample_rate
        self.h = L.sfizz_create_synth()
        L.sfizz_set_sample_rate(self.h, float(sample_rate))
        L.sfizz_set_samples_per_block(self.h, block)
        L.sfizz_enable_freewheeling(self.h)  # load samples synchronously: no streaming dropouts offline
        L.sfizz_set_num_voices(self.h, voices)
        if not L.sfizz_load_file(self.h, os.path.abspath(sfz_path).encode("utf-8")):
            raise RuntimeError(f"sfizz failed to load {sfz_path}")
        self.num_regions = L.sfizz_get_num_regions(self.h)

    def close(self):
        if self.h:
            lib().sfizz_free(self.h)
            self.h = None

    def render(self, events, total_samples, stop_on_silence_after=None):
        """events: list of (sample_time, kind, a, b) with kind in {'on','off','cc','pb'}.

        Returns float32 array (n, 2). If stop_on_silence_after is set, rendering stops early once
        no voices are active past that sample position.
        """
        L = lib()
        # same sample: CCs, then note-offs, then keyswitches (notes >= 120), then note-ons
        events = sorted(events, key=lambda e: (e[0], {"cc": 0, "pb": 0, "off": 1, "on": 2}[e[1]],
                                               0 if (e[1] == "on" and e[2] >= 120) else 1))
        n_blocks = (total_samples + self.block - 1) // self.block
        out = np.zeros((2, n_blocks * self.block), dtype=np.float32)
        bufs = [np.zeros(self.block, dtype=np.float32) for _ in range(2)]
        ptrs = (ctypes.POINTER(ctypes.c_float) * 2)(*[b.ctypes.data_as(ctypes.POINTER(ctypes.c_float)) for b in bufs])
        ei = 0
        last_block = n_blocks
        for bi in range(n_blocks):
            start = bi * self.block
            end = start + self.block
            while ei < len(events) and events[ei][0] < end:
                t, kind, a, b = events[ei]
                d = max(0, int(t) - start)
                if kind == "on":
                    L.sfizz_send_note_on(self.h, d, int(a), int(b))
                elif kind == "off":
                    L.sfizz_send_note_off(self.h, d, int(a), int(b))
                elif kind == "cc":
                    L.sfizz_send_cc(self.h, d, int(a), int(b))
                elif kind == "pb":
                    L.sfizz_send_pitch_wheel(self.h, d, int(a))
                ei += 1
            for b_ in bufs:
                b_[:] = 0.0
            L.sfizz_render_block(self.h, ptrs, 2, self.block)
            out[0, start:end] = bufs[0]
            out[1, start:end] = bufs[1]
            if (stop_on_silence_after is not None and start > stop_on_silence_after and ei >= len(events)
                    and L.sfizz_get_num_active_voices(self.h) == 0):
                last_block = bi + 1
                break
        return out[:, : min(total_samples, last_block * self.block)].T.copy()


def midi_track_to_events(notes, ccs, sr, pitch_bends=()):
    """notes: iterable of (start_s, end_s, pitch, velocity); ccs: iterable of (time_s, cc, value)."""
    ev = []
    for s, e, p, v in notes:
        ev.append((int(round(s * sr)), "on", p, v))
        ev.append((int(round(e * sr)), "off", p, 0))
    for t, c, val in ccs:
        ev.append((int(round(t * sr)), "cc", c, val))
    for t, val in pitch_bends:
        ev.append((int(round(t * sr)), "pb", val, 0))
    return ev
