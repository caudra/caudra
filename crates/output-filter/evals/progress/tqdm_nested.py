#!/usr/bin/env python3
"""Nested tqdm bars, which emit cursor-movement escapes as well as redraws.

This is the case that decides whether single-row reduction is sufficient: nested
bars move the cursor between rows rather than redrawing one row in place.
"""

import time

from tqdm import tqdm

for _ in tqdm(range(3), desc="epoch"):
    for _ in tqdm(range(80), desc="batch", leave=False):
        time.sleep(0.002)
