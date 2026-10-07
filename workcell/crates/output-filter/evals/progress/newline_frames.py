#!/usr/bin/env python3
"""Progress frames separated by newlines rather than carriage returns.

Loggers, CI log collectors, and container runtimes routinely convert a redraw
stream into one line per frame. Row reduction cannot help there, because every
frame is already its own row. This is the input that the line-collapse stage
exists for, and it is surrounded by ordinary output that must survive.
"""

import sys

print("loading checkpoint shards")
for done in range(0, 851, 50):
    pct = done * 100 // 851
    filled = "\u2588" * (pct // 10)
    sys.stdout.write(
        f"Loading weights: {pct:>3}%|{filled:<10}| {done}/851 [02:03<02:06,  4.82it/s]\n"
    )
print("Loading weights: 100%|\u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588| 851/851 [04:21<00:00,  3.26it/s]")
print("checkpoint loaded")
