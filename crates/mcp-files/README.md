# workcell-mcp-files

`workcell-mcp-files` implements the six first-party filesystem MCP tools. It owns filesystem behavior,
the reviewed MCP catalog projection, structured results, bounded previews, and tool dispatch. It does
not own transport, process configuration, or host permission decisions.

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

## Compatibility

Shared fixtures under `fixtures/mcp-conformance` define deterministic catalog, model-text, structured
output, and post-filesystem expectations. The deliberate exception is Rust grep's linear-time subset,
which is disclosed in the catalog and tested independently from the legacy JavaScript-regex wording.

## Verification

```bash
cargo fmt --all --check
cargo clippy -p workcell-mcp-files --all-targets -- -D warnings
cargo test -p workcell-mcp-files
```
