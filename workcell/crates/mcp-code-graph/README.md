# workcell-mcp-code-graph

`workcell-mcp-code-graph` is Workcell's repository-scale code tool group: five tools over one graph
built from a confined source tree.

| Tool | Answers |
| --- | --- |
| `code_map` | orient: what are the important symbols here |
| `code_context` | what should I read before making this change |
| `code_refs` | what references this, or what does this reference |
| `code_impact` | what breaks if I change this, and what tests cover it |
| `code_expand` | show me this symbol and what sits next to it |

The ranking engine lives in `workcell-code-graph`. This crate owns the crawl, the tool schemas, the
result shaping, and the MCP projection.

## Confinement

This crate never opens a path. It is constructed over a `workcell-mcp-files` group and every read
goes through it, so there is exactly one path resolver and one confinement implementation in the
process. `CodeGraphToolGroup::from_files` shares a group a host already built rather than creating a
second one with possibly different confinement, which would be two answers to one authorization
question.

Nothing here sandboxes anything. The tools only read, and they read only what the filesystem group
would already return.

## Honesty vocabulary

Call edges are recovered from source text by name, so every count is a **floor**. That is a result
field, not only a sentence in a description:

- `complete` / `truncated_by` — which bound fired, named, when one did.
- `confidence` on `code_context` — derived from how far the top result separates from the rest. A
  measure of separation, never a claim of correctness; a single result is always low.
- `SelectorRefusal` — an unknown symbol is refused with did-you-mean candidates, not answered with
  zero. A symbol that exists and has no callers returns zero. The two are different answers.
- `ambiguous` — a name matching several definitions returns their union and says so, rather than
  silently picking one.

## Output convention

Every tool returns a text block and `structuredContent`, and neither restates the other: the text is
the model-facing rendering, the structured record is canonical. Results are fitted to a 64,000-byte
envelope by binary search over retained rows, measured on the serialized envelope rather than
estimated.

## Benchmarks

`examples/code_map_bench.rs` times one `code_map` call over a real tree, and
`evals/compare-ripwire.sh` runs it beside the upstream binary:

```bash
cargo build --release --example code_map_bench -p workcell-mcp-code-graph
crates/mcp-code-graph/evals/compare-ripwire.sh <tree> [<tree> ...]
```

The upstream `ripwire` binary keeps an **on-disk cache** under `/tmp/ripwire-<uid>`, so its second run
over a tree is largely a cache hit. This crate has no persistent cache: its fact cache lives in the
process and dies with it.

The benchmark is therefore deliberately unfair to us, and that is the point. It measures ripwire
**with** its cache warmed against this pipeline with **no** cache, so the number in the first column
is a standing target rather than a like-for-like result. The harness builds a fresh group per run and
leaves the per-file hashing in, because the production ingest is the cached one and a cold run is all
misses.

Medians of seven runs on a 32-core Linux host against `ripwire 0.4.0`:

| Tree | ripwire, cache warm | this, no cache | gap to close | ripwire, cache cleared |
| --- | --- | --- | --- | --- |
| ripwire's own C++ source, 153 files / 8.6 MB | **117 ms** | 377 ms | 3.2x | 596 ms |
| this workspace's Rust crates, 691 files | **72 ms** | 226 ms | 3.1x | 501 ms |
| ripwire's test corpus, 1154 small files | **103 ms** | 331 ms | 3.2x | 533 ms |

Only the middle row was re-measured when repository-boundary pruning and `.gitignore` support landed;
its tree had also grown from 359 files to 691 since the first measurement. Measured immediately
before and after that change on the same host, the medians were 220 ms and 210 ms, so the filters are
cost-neutral within run-to-run noise: they add one `open` per directory that has an ignore file and
no syscall at all to detect either a boundary or an ignore file, since both are already in the
directory scan.

Read it two ways. Against a cache we do not have, we are 3.1–3.2x behind, and closing that is what a
persistent cache would have to buy. Against the same pipeline doing the same work — the last column,
ripwire's cache cleared before every run — we are 1.6–2.2x ahead, so the gap is the cache and not the
ranking.

Numbers from one host are not a portability claim, and the last column moves with page-cache state.
Re-run the harness before quoting any of them, and clear `/tmp/ripwire-<uid>` between cold runs or
that column silently becomes the first one.
