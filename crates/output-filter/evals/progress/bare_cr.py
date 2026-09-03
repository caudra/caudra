#!/usr/bin/env python3
"""Hand-rolled carriage-return progress with no library and no timing.

Deterministic on purpose: this fixture is asserted byte-for-byte, so it pins the
reduction itself rather than a property of whatever tqdm happened to print.
Frames shrink in width partway through so the fixture also covers the case where
a shorter frame must not leave residue from a longer one behind it.
"""

import sys

WIDTH = 51

sys.stdout.write("preparing 3 inputs\n")
for step in range(0, 101, 5):
    bar = "#" * (step // 5)
    frame = f"extracting [{bar:<20}] {step:>3}% of 100 files"
    assert len(frame) == WIDTH, len(frame)
    sys.stdout.write("\r" + frame)
# A well-behaved writer pads its last frame over the widest one it drew, so the
# residue is trailing blanks rather than leftover text.
sys.stdout.write("\r" + "extracting complete".ljust(WIDTH))
sys.stdout.write("\n")
sys.stdout.write("wrote 3 outputs\n")
