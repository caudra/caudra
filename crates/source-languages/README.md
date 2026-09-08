# workcell-source-languages

`workcell-source-languages` owns the single `extension -> {grammar, tags query}` table Workcell
parses with. It has no protocol dependency and no filesystem access: callers hand it a path or an
extension and receive a language, a tree-sitter grammar, and a compiled tags query.

Two consumers share it. `workcell-mcp-files` needs detection and a grammar for the `file_index` tool.
`workcell-code-graph` additionally needs the tags query and the capture-role mapping. One table is
what stops the two from disagreeing about what a `.tf` file is.

## Public API

```rust,no_run
use workcell_source_languages::{Language, LanguageFamily};

let language = Language::from_path(std::path::Path::new("src/main.rs")).expect("rust");
assert_eq!(language.name(), "rust");
assert_eq!(language.family(), LanguageFamily::Code);
let _grammar = language.grammar();
let _query = language.tags_query();
```

`ALL` lists every language in declaration order. `tags_query` compiles once per language for the
life of the process and returns a shared reference; every file of that language reuses it.

## Extraction Model

Extraction is query-driven, never a hand-written AST walk per language. Each language contributes
one `queries/<language>/tags.scm`, and `CaptureRole::classify` maps a capture name to a role:

| Capture | Role |
| --- | --- |
| `@definition.function`, `.method`, `.class`, `.interface`, `.module`, `.macro`, `.constant`, `.field`, `.section` | a symbol, with the capture node's span |
| `@reference.call`, `.import`, `.extends`, `.read` | a use site |
| `@name` | the identifier token for whichever definition or reference it accompanies |
| `@doc`, `@qualifier` | an attached doc comment; the leading segment of a qualified reference |

Aliases exist so a query can use the vocabulary its language community already writes:
`definition.struct`/`enum`/`type` record as `class`, `definition.trait`/`protocol` as `interface`,
`definition.namespace`/`package` as `module`. An unrecognized capture name is ignored rather than
rejected, because a query may name a node purely to make a pattern readable.

`SymbolKind` is ordered by specificity, not alphabetically. When two patterns capture the same name
token, the higher kind wins, which is what makes a method inside an `impl` record as a method rather
than losing to whichever pattern matched second.

## Invariants

- Queries are `include_str!` build inputs. There is no runtime asset path, override environment
  variable, or per-project query directory: the extractor a build ships with is the extractor it
  runs.
- A `@definition.*` capture sits on the node that owns the body, never on a wrapper list node. A
  wrapper has no `body:` field, so span widening climbs to the enclosing block and every definition
  in it inherits the block's span. Enclosing attribution then resolves every call in the block to
  one arbitrary member. `rust_captures_the_exact_shape_the_span_fix_depends_on` pins this.
- Every `@definition.*` pattern also captures a `@name`. A definition that cannot be addressed by
  name cannot be ranked or resolved against, so an unnamed one is a pattern bug, not a partial
  result.
- `LanguageFamily::Config` and `LanguageFamily::Prose` emit no call references. A YAML key is data;
  an edge minted from one would assert control flow that no execution performs.
- `compatible_with` refuses cross-language resolution. TypeScript/JavaScript and the three Bazel
  file shapes are mutually compatible because each pair shares a grammar and a module system.
  Everything else is refused, so a YAML `deploy` key and a Go `deploy` function stay distinct.

## Adding a Language

1. Add the variant, and rows in `ALL`, `from_extension`, `name`, `grammar`, and `family`.
2. Add `queries/<language>/tags.scm` and a row in `src/queries.rs`.
3. Add exactly one fixture under `tests/fixtures/` that `Language::from_path` resolves to the new
   variant. The shared harness discovers fixtures by that rule, so no test file is edited.

Use the dump example to find the real node kinds and field names, which differ between grammar
versions and are not documented anywhere authoritative:

```bash
cargo run -p workcell-source-languages --example dump -- path/to/file.rs
cargo run -p workcell-source-languages --example dump -- path/to/file.rs --captures
```

## Verification

```bash
cargo test -p workcell-source-languages
```

`every_query_compiles_against_its_grammar` is the grammar-bump canary: a crates.io update that
renames or removes a node kind fails there with the language named. `Query::new` succeeding proves
only that the node kinds exist, so the fixture-driven tests in `tests/extraction.rs` carry the rest
of the weight — a query that compiles and captures nothing would otherwise render a confident,
empty map.

## Attribution

Sixteen queries are seeded from [ripwire](https://github.com/redhat-et/ripwire) (Apache-2.0), which
derived them in turn from the upstream tree-sitter grammar repositories (MIT). Each file names its
provenance and any deliberate divergence in its header. See `THIRD_PARTY.md`.
