#!/usr/bin/env python3
"""Repeated warnings that must NOT be collapsed.

These lines are shape-identical and would fall to naive deduplication, but they
carry no progress signal and no monotonic counter. Diagnostics are the part of
the output a caller needs, so this fixture pins that they survive whole.
"""

import sys

for name in ("alpha", "beta", "gamma", "delta", "epsilon"):
    sys.stderr.write(f"warning: unused variable `{name}` in module 7\n")
