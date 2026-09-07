# Third-Party Material

Workcell is Apache-2.0. This file records third-party material vendored into the source tree, as
distinct from crates resolved by Cargo, whose licences are recorded in `Cargo.lock` and audited by
the dependency check.

## Vendored tree-sitter tags queries

`crates/source-languages/queries/<language>/tags.scm`

Sixteen queries — bash, c, c_sharp, cpp, go, java, json, lua, php, python, ruby, rust, swift, toml,
typescript, yaml — are seeded from [ripwire](https://github.com/redhat-et/ripwire), Apache-2.0,
which derived them in turn from the upstream tree-sitter grammar repositories, MIT. Each file states
its provenance and any deliberate divergence from upstream in its header.

They are not copies. Every seeded query was re-verified against the grammar version vendored here,
which differs from ripwire's, and capture names were normalized to the vocabulary in
`crates/source-languages/src/roles.rs`. The divergences ripwire documents — notably the Rust
`@definition.method` span fix and the `::`-path and turbofish call patterns upstream lacks — are
carried forward with their reasoning intact, because the reasoning is what makes them reviewable.

The remaining fifteen — containerfile, css, dart, elixir, gleam, hcl, html, kotlin, make, markdown,
nix, scala, sql, starlark, zig — are authored here against the vendored grammars and carry no
upstream provenance.

## Output filter rules

`crates/output-filter/rules/`

Vendored verbatim from RTK. The corpus must stay byte-identical so a refresh is a clean copy;
rules authored here live in `crates/output-filter/rules-workcell/` instead.

## Monty worker

The `monty` worker binary is installed from a pinned upstream release rather than built with the
workspace, and is not vendored into this tree. `crates/monty-worker` embeds the bytes of that
release at build time when `WORKCELL_BUNDLED_MONTY_WORKER` is set.
