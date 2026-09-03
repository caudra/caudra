#!/usr/bin/env python3
"""A tqdm bar routed to stdout, interleaved with real stdout content.

The interleaving matters: it proves the reduction keeps ordinary lines printed
before and after the bar instead of collapsing the whole stream.
"""

import sys
import time

from tqdm import tqdm

total = int(sys.argv[1]) if len(sys.argv) > 1 else 300
print("resolved 12 shards")
print("dtype=bfloat16")
for _ in tqdm(range(total), desc="Fetching shards", file=sys.stdout):
    time.sleep(0.004)
print("done in 1.2s")
