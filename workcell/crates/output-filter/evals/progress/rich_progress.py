#!/usr/bin/env python3
"""A `rich` progress bar, which consults isatty where tqdm does not.

Measured rather than assumed. On a pipe `rich` renders once at the end instead
of redrawing, so it produces a single line and needs no reduction. Keeping it in
the capture set records that, so the claim stays evidence rather than memory.
"""

import time

from rich.progress import Progress

with Progress() as progress:
    task = progress.add_task("Loading weights", total=200)
    for _ in range(200):
        progress.advance(task)
        time.sleep(0.002)
