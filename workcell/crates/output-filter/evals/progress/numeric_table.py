#!/usr/bin/env python3
"""A numeric table that must NOT be collapsed.

Consecutive rows here share a shape and carry a monotonic counter with a
constant denominator, so shape and monotonicity alone would destroy it. Only the
requirement for two independent progress signals keeps it intact. This is the
negative fixture for the line-collapse stage.
"""

print("step  loss  batch")
for step in range(1, 13):
    print(f"{step:>4}  {1.0 / step:.3f}  {step}/12")
