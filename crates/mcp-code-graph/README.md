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

`examples/code_map_bench.rs` times one `code_map` call over a real tree:

```bash
cargo run --release --example code_map_bench -- <root> 5
```

It reports cold and warm separately, because the upstream `ripwire` binary this group was ported from
keeps an on-disk cache and comparing our cold against its warm measures the cache and not the
pipeline. Medians on a 32-core Linux host against `ripwire 0.4.0`, its cache cleared for cold runs
and retained for warm ones:

| Tree | ripwire cold | this cold | ripwire warm | this warm |
| --- | --- | --- | --- | --- |
| ripwire's own C++ source, 153 files / 8.6 MB | 590 ms | **380 ms** | 118 ms | **89 ms** |
| this workspace's Rust crates, 357 files | 430 ms | **167 ms** | 55 ms | 92 ms |

Cold is the honest comparison of the pipeline and we win it on both trees. Warm is a comparison of
caches: ours retains extraction facts in process by content digest and still re-reads and re-hashes
the tree on every call, which is why a tree of many small files can favour an on-disk cache that does
not. That is a deliberate trade — a stat-triple cache would be faster warm and would answer from
metadata rather than content.

Numbers from one host are not a portability claim. Re-run the harness before quoting them.
