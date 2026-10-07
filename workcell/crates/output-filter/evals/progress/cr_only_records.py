#!/usr/bin/env python3
"""Carriage-return-separated records with no newline anywhere.

Classic Mac line endings are the known false-positive class for row reduction: a
terminal really would overwrite these into a single row, so reduction is
faithful but destructive. This fixture exists so the behaviour is pinned and
visible rather than discovered later.
"""

import sys

sys.stdout.write("alpha\rbeta\rgamma\rdelta")
