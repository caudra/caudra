# Third-Party Material

Workcell is Apache-2.0. This file records third-party material incorporated into its source tree.
The repository-root `Cargo.lock` records dependency resolutions, not license texts. Build attribution
from `scripts/build-attribution.py` supplements these notices. Neither the lockfile nor generated
attribution establishes complete license or embedded-asset coverage.

License texts referenced below live in the repository-root `THIRD_PARTY_LICENSES/` directory.

## Vendored tree-sitter tags queries

`crates/source-languages/queries/<language>/tags.scm`

Seventeen queries (bash, c, c_sharp, cpp, go, java, json, lua, objc, php, python, ruby, rust, swift,
toml, typescript, yaml) are adapted from [ripwire](https://github.com/redhat-et/ripwire), Apache-2.0.
Their headers identify upstream tree-sitter queries or grammar node types used by ripwire, under MIT.
`Ripwire.txt` preserves ripwire's license and copyright. `TreeSitterQueries.txt` preserves the MIT
texts and copyright notices for all seventeen upstream grammars, with source revisions.
CUDA has no query of its own: its grammar extends C++'s, and it shares `cpp`. Objective-C's grammar
extends C's, and `objc` holds only the Objective-C layer that is appended to `c`.

These are modified copies or adaptations, not independent originals. Every seeded query was
re-verified against the grammar version used here, which differs from ripwire's, and capture names
were normalized to the vocabulary in `crates/source-languages/src/roles.rs`. The divergences ripwire
documents, notably the Rust `@definition.method` span fix and the `::`-path and turbofish call patterns
upstream lacks, are carried forward with their reasoning intact.

The original ripwire import revision was not recorded in the headers or import commit. The notice
material records an inspected snapshot, `255dc199c54e4c6c43e25b26a713154a84efd6ae`, whose
`THIRD_PARTY.md` pins all seventeen upstream grammars. It is not asserted to be the import revision.
The versioned origins in the query headers agree with that snapshot. Swift and TypeScript headers
do not name versions, so their notice sources use its grammar pins. Resolving the original import
revision remains a provenance gap.

The remaining eighteen (cmake, containerfile, css, dart, elixir, gleam, hcl, html, kotlin, make,
markdown, nix, proto, scala, sql, starlark, xml, zig) are authored here against the vendored
grammars and carry no upstream provenance.

## Code-graph pipeline

`crates/code-graph/`

The ranking pipeline (the resolution ladder, personalized PageRank over an in-edge graph, the BM25
lexical lane, and their reciprocal-rank fusion) is a reimplementation of the corresponding stages of
[ripwire](https://github.com/redhat-et/ripwire), Apache-2.0. No ripwire implementation source is
vendored in this pipeline: it is C++, this is Rust, and the data structures, bounds, cache, and honesty
vocabulary are our own. The debt is to the design, recorded here to explain the connection.

Ripwire's on-disk cache blob, its write verbs, its own transport, and its remaining verb families are
deliberately not ported. `crates/mcp-code-graph/README.md` records the benchmark comparison against
the upstream binary.

## Output filter rules

`crates/output-filter/rules/`

Vendored verbatim from RTK. The corpus must stay byte-identical so a refresh is a clean copy.
Rules authored here live in `crates/output-filter/rules-workcell/` instead.

Source: `rtk-ai/rtk` revision `aa408534859949ebac1dcc82ec4d25b575a539fa`,
`src/filters/*.toml` (tag `dev-0.47.0-rc.386`). `RTK.txt` preserves that revision's `LICENSE`,
including `Copyright 2024 rtk-ai and rtk-ai Labs`. No root upstream `NOTICE` exists at that revision.
`crates/output-filter/NOTICE` records the incorporated material and local behavioral differences.

## Monty worker

The `monty` worker binary is installed from a pinned upstream release rather than built with the
workspace, and is not vendored into this tree. `crates/monty-worker` embeds the bytes of that
release at build time when `WORKCELL_BUNDLED_MONTY_WORKER` is set.
