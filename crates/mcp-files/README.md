# workcell-mcp-files

`workcell-mcp-files` implements the six base filesystem tools and the optional `index` tool. It owns
filesystem behavior, the reviewed MCP catalog projection, structured results, bounded previews, and
tool dispatch. It does not own transport, process configuration, or host permission decisions.

## Public API

The primary composition type is `FileToolGroup`:

```rust,no_run
use workcell_mcp_files::FileToolGroup;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let files = FileToolGroup::new(".", false, None).await?;
let catalog = files.catalog();
assert_eq!(catalog.len(), 6);
# Ok(())
# }
```

`FileToolGroup::new` accepts a root, an `allow_write` flag, and optional `FilesystemLimits`. The group
is cloneable; clones share immutable root policy and a per-service mutation lock. Typed operation
methods are available alongside JSON MCP dispatch.

## Tools

| Tool               | Behavior                                                                   |
| ------------------ | -------------------------------------------------------------------------- |
| `file_read`        | Bounded file windows and deterministic directory listings.                 |
| `file_glob`        | Bounded `*`, `**`, `?`, and brace matching with UTF-16 wildcard semantics. |
| `file_grep`        | Linear-time regex search over bounded UTF-8 text.                          |
| `file_write`       | Dry-run diff or atomic same-directory replacement.                         |
| `file_edit`        | Exact unique replacement, optional replace-all, and dry-run diff.          |
| `file_apply_patch` | Validated add, update, move, and delete sections with bounded results.     |
| `index`            | Feature-gated source skeletons and deterministic typed directory listings. |

`index` is not part of the base `files` build. Enable the crate's `index` feature, or the facade's
`files-index` feature, to compile its parser bundle and expose the tool. Native execution uses
`index` with secure defaults or `index_with_configuration` with host-only `IndexLimits`; limits are
not accepted in model input. `inspect_index` resolves exactly one existing file (`Read`) or directory
(`Traverse`) without opening source content.

Language extraction, import merging, section ordering, formatting, and range metadata are native Rust
tree-sitter visitors. The feature has no scripting interpreter or runtime-loaded extractor assets.

Grep supports common alternation, grouping, classes, anchors, and repetition. Look-around and
backreferences are rejected because preserving those JavaScript constructs would permit catastrophic
backtracking controlled by model input.

## Filesystem Invariants

- The root must exist and be a directory.
- Lexical escapes, stable symlink escapes, and protected paths are rejected.
- Broad traversal skips symlinks, `.git`, `node_modules`, and protected entries.
- Binary files are rejected using content signatures and a bounded byte-sample fallback; filename
  extensions do not control classification.
- Reads, lines, traversal, regexes, globs, patches, diffs, plans, and MCP responses have independent
  limits.
- Index source, model text, output lines, directory entries/scans, admission, parser wall time, and
  process-wide concurrency have independent limits. Node and depth limits apply during post-parse
  inspection and extraction; they do not cap tree-sitter construction memory.
- Index directory `totalCount` is exact for complete scans and a lower bound on processed visible entries
  when `truncated` is true.
- Mutations are read-only by default; dry-run validation remains available.
- Existing mode bits are preserved and new files use mode `0600` on supported platforms.
- Replacements use exclusive same-directory temporary files and atomic rename.
- Source identity and content are revalidated before edit or patch publication.
- Patch output is constructed and checked against the host's 64,000-byte MCP result ceiling before
  the first file is published.

Multi-file publication is validated as a unit but is not transactional after publication starts. A
later I/O failure can leave earlier sections applied, and this behavior has an explicit regression
test.

## Security Boundary

The crate is a bounded local executor, not a sandbox. Canonical path checks prevent stable escapes but
ambient pathname operations have a malicious symlink-swap TOCTOU window between authorization and I/O.
A complete fix requires retaining descriptor-relative capability handles through reads, writes,
renames, and deletes, or running inside an OS sandbox.

## Result Shape

Every tool result carries two forms. The content block is the model-facing rendering: numbered lines
for a file read, a listing for a directory read, relative paths for `file_glob`, `path:line: text`
rows for `file_grep`, a skeleton or listing for `index`, and a unified diff for every mutation. The
structured content is the canonical record a program consumes.

Neither form restates the other. A field is omitted from the structured record only when it is
exactly derivable from a field that remains, so `numberedText` follows from `text` and `lineStart`, a
directory listing from its entry details, the combined `file_apply_patch` `diff` from the per-file
patches, and the `index` `skeleton` and `listing` from `lines` and `entries`. Native callers still read every field on the Rust types; only the serialized record
is deduplicated. Both forms are charged against the protocol ceiling and the configured result
budgets.

## Compatibility

Shared fixtures under `fixtures/mcp-conformance` define deterministic catalog, model-text, structured
output, and post-filesystem expectations. The deliberate exception is Rust grep's linear-time subset,
which is disclosed in the catalog and tested independently from the legacy JavaScript-regex wording.

## Verification

```bash
cargo fmt --all --check
cargo clippy -p workcell-mcp-files --all-targets --features index,mcp -- -D warnings
cargo test -p workcell-mcp-files --features index,mcp
```
