#!/usr/bin/env python3
"""A tqdm bar on stderr, the shape emitted by transformers, vllm, and safetensors.

tqdm does not consult isatty by default, so this emits carriage-return redraw
frames even when stderr is a pipe. That is what makes it the dominant source of
redraw noise in captured command output.
"""

import sys
import time

from tqdm import tqdm

total = int(sys.argv[1]) if len(sys.argv) > 1 else 400
for _ in tqdm(range(total), desc="Loading weights"):
    time.sleep(0.004)
