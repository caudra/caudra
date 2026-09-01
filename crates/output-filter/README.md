# workcell-output-filter

`workcell-output-filter` applies a declarative rule set to captured command output so a bounded
model-facing window carries proportionally more signal. It performs no I/O, spawns no processes, and
reads no configuration from disk.

## Corpus

Rules live in two directories, both compiled into the binary at build time by `build.rs`:

- `rules/` is vendored verbatim from RTK. Keep it byte-identical so a refresh is a clean copy.
- `rules-workcell/` holds rules authored in this project, covering common build, test,
  package-manager, and container commands.

A file name may not appear in both directories, and every rule name must be unique across them; the
build fails otherwise. Match patterns should be mutually exclusive, because `Corpus::find` returns the
first match in rule-name order.

## Authoring a rule

Write a rule against output captured from the real tool, never from a remembered format. The harness
in `evals/` builds a container with the relevant tools installed, runs sample projects through the
actual server, and reports the reduction and whether any command was made larger:

```bash
docker build -t workcell-eval:local crates/output-filter/evals
```

Every rule must ship inline `[[tests.<name>]]` expectations covering what it strips, that diagnostics
and errors survive, and any success message. `cargo test -p workcell-output-filter` executes them.

Match only the subcommands whose output is progress. When a subcommand exists to produce the listing
it prints, such as `tar -t`, `apt list`, or `docker compose logs`, leave it unmatched: stripping there
removes the answer rather than the noise. Prefer `strip_lines_matching` over `keep_lines_matching`
unless every diagnostic the tool can emit is known to share a prefix, because an unrecognized line
should survive by default. When a header is dropped, drop its indented continuation too; keeping one
without the other leaves an orphan that reads worse than the original.

Measure before writing. A tool whose output is diagnostics rather than progress has nothing to strip,
and a rule for it would only add risk: `tsc`, `eslint`, and `golangci-lint` were each measured in the
harness at zero reduction and deliberately have no rule. Capping or deduplicating diagnostics is not
a substitute, because it discards the part of the output the caller needs.

Watch for a line prefix that has both a success and a failure form. `wget` writes
`Resolving host... 1.2.3.4` and `Resolving host... failed: ...` alike, so a pattern anchored on the
prefix alone would delete the error. Match the success form specifically.

Write digit classes as `[0-9]` rather than `\d` in a bounded repetition. `\d` is Unicode-aware, and
repeating it across a timestamp compiles past the program-size bound.

## Contract

`builtin()` returns the process-wide compiled corpus. `Corpus::find` selects at most one rule by
matching a **normalized command scope**; `Rule::apply` renders the captured text.

```rust
if let Some(rule) = workcell_output_filter::builtin().find(normalized_scope) {
    let filtered = rule.apply(stdout, stderr, exit_code);
}
```

Callers pass a normalized scope, not a raw request string. Path form, quoting, and leading
assignments therefore cannot select a rule the operator did not intend, nor evade one.

## Pipeline

Stages run in a fixed order; later stages assume earlier ones have run.

1. `strip_ansi` — remove escape sequences
2. `replace` — chained per-line substitutions
3. `match_output` — short-circuit to a message, skipped when `unless` also matches
4. `strip_lines_matching` **or** `keep_lines_matching` — never both
5. `truncate_lines_at` — per-line cap, counted in characters
6. `head_lines` / `tail_lines` — windowing with omission markers
7. `max_lines` — absolute cap, applied after windowing so markers are counted
8. `on_empty` — message when nothing survived

## Invariants

- Filtering is a rendering step. It is not a substitute for retaining the raw capture, and callers
  are expected to keep one.
- A command that exited non-zero never has its output replaced by a success message. Stages 3 and 8
  are gated on a zero exit status, because a caller cannot distinguish a synthetic `ok` from a real
  one. This diverges deliberately from upstream.
- Only the built-in corpus is loaded. Rule files discovered on disk are unsupported: a rule read
  from the tree under inspection could rewrite what a model sees.
- Rule count, pattern count, pattern length, compiled program size, input bytes, and input lines are
  all bounded. Compilation happens once per process, never per call.
- Input beyond the processing bound is reduced to its tail, matching the capture ring's rationale
  that failures and summaries appear last.

## Refreshing the vendored rules

`rules/*.toml` are vendored verbatim from RTK; see `NOTICE` for provenance. Each file ships inline
expectations that `tests/corpus.rs` executes, so refreshing them is self-verifying: copy the files in
and run the suite.

Decoding is strict. A refresh that introduces an unrecognized field fails at load rather than
silently dropping a transformation.

## Verification

```bash
cargo clippy -p workcell-output-filter --all-targets -- -D warnings
cargo test -p workcell-output-filter
```
