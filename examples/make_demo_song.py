#!/usr/bin/env python3
"""Write examples/demo.m8s: a short three-track loop built on the empty
DEFAULT.m8s from m8-js (tests/fixtures), using the 2.x song layout."""

import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
song = bytearray((ROOT / "tests/fixtures/DEFAULT.m8s").read_bytes())

ROWS, PHRASES, CHAINS, INSTRUMENTS = 0x2EE, 0xAEE, 0x9A5E, 0x13A3E
C3, C4, C5 = 24, 36, 48  # M8 notes: 0 is C-1


def put_str(off, text, size):
    song[off:off + size] = text.encode().ljust(size, b"\0")


def step(phrase, i, note, inst=0xFF, vol=0xFF, fx=None):
    off = PHRASES + phrase * 144 + i * 9
    song[off:off + 3] = bytes([note, vol, inst])
    if fx:
        song[off + 3:off + 5] = bytes(fx)


def chain(n, steps):
    for i, (phrase, transpose) in enumerate(steps):
        off = CHAINS + n * 32 + i * 2
        song[off:off + 2] = bytes([phrase, transpose & 0xFF])


def instrument(n, kind, name):
    off = INSTRUMENTS + n * 215
    song[off] = kind
    put_str(off + 1, name, 12)
    song[off + 13] = 1  # chain transpose applies
    song[off + 14:off + 18] = bytes([1, 0xE0, 0, 0x80])  # table tick, volume, pitch, fine tune


put_str(148, "DEMO", 12)
song[143:147] = struct.pack("<f", 118.0)

instrument(0, 0x00, "BASS")  # wavsynth
instrument(1, 0x01, "ARP")   # macrosynth
instrument(2, 0x04, "PAD")   # fmsynth

# Phrase 00: bass line, notes cut short with KIL (05).
for i, n in [(0, C3), (3, C3), (6, C3 + 12), (8, C3), (11, C3 + 10), (14, C3 + 7)]:
    step(0, i, n, inst=0x00, vol=0x60, fx=(0x05, 0x04))
# Phrase 01: arpeggio, every other step.
for i, n in enumerate([C5, C5 + 3, C5 + 7, C5 + 10, C5 + 12, C5 + 10, C5 + 7, C5 + 3]):
    step(1, i * 2, n, inst=0x01, vol=0x50)
# Phrase 02: one long pad chord root per phrase, the last step delayed (DEL 02).
step(2, 0, C4, inst=0x02, vol=0x40)
step(2, 12, C4 + 7, vol=0x30, fx=(0x02, 0x03))

chain(0x00, [(0x00, 0), (0x00, 0)])
chain(0x01, [(0x00, 5), (0x00, 7)])
chain(0x02, [(0x01, 0), (0x01, 0)])
chain(0x03, [(0x01, 5), (0x01, 7)])
chain(0x04, [(0x02, 0), (0x02, 0)])
chain(0x05, [(0x02, 5), (0x02, 7)])

# Song: rows 00-01 loop (row 02 is empty).
for row, cells in [(0, [0x00, 0x02, 0x04]), (1, [0x01, 0x03, 0x05])]:
    song[ROWS + row * 8:ROWS + row * 8 + 3] = bytes(cells)

(ROOT / "examples/demo.m8s").write_bytes(song)
print("wrote examples/demo.m8s")
