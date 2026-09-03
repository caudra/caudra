#!/usr/bin/env python3
"""Carriage-return redraw that erases with CSI K instead of padding with spaces.

This distinguishes correct overwrite semantics from naive last-segment-wins. The
frames get shorter without padding, so a reducer that keeps only the text after
the final carriage return is right here only because the erase was honoured. The
final line deliberately carries no erase, so residue from the previous frame
must survive exactly as a terminal would show it.
"""

import sys

for step in (5, 40, 100):
    sys.stderr.write(f"\rdownloading {step}% [{'=' * (step // 10):<10}]\x1b[K")
sys.stderr.write("\rdone")
sys.stderr.write("\n")
