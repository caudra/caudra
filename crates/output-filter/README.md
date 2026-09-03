# workcell-output-filter

`workcell-output-filter` renders captured command output so a bounded model-facing window carries
proportionally more signal. It does three things: it decodes a terminal redraw stream into the rows a
terminal would show, it applies a declarative rule set selected by command, and it collapses progress
frames that arrive one per line. It performs no I/O, spawns no processes, and reads no configuration
from disk.

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

## Progress

Two reductions are command-independent, because the programs that emit progress
bars are overwhelmingly ones no rule names — a training script, an ad-hoc
downloader — and a rule cannot be written for a program that does not exist yet.
Neither is selected by `match_command`.

`render_terminal` and `RowRenderer` decode a redraw stream. A bar that redraws
does not emit lines; it emits `\r`, overwriting text, and erase sequences, so
`str::lines` sees one row that can be hundreds of kilobytes wide. Every stage
above is defeated by that: `truncate_lines_at` keeps the opening frame and
discards the final one, and `max_lines` counts the whole bar as one line. The
rendered row is what a terminal would display, which is what the writer intended
a reader to see, so this is decoding rather than filtering and is not gated on a
rule or on the filter being enabled. Scope is one row; sequences that move
between rows are consumed, and sequences with no cursor meaning are treated as
zero width so they never shift a coloured frame's overwrite alignment.

`collapse_progress_lines` handles frames that already arrive one per line, which
is what a logger or a CI log collector produces from the same stream. A run is
collapsed only when all three of shape, signal, and advance agree — see the
module documentation. Each gate removes a distinct false positive, and each has
a fixture: `numeric-table` survives because of the signal count, and
`repeated-diagnostic` because of the advance requirement.

Both are measured by the same harness as the rules, and the progress cases run
commands the corpus deliberately does not name.

## Fixtures

`tests/fixtures/progress/*.raw` are the exact bytes real tools wrote to a pipe.
Regenerate them with:

```bash
python3 crates/output-filter/evals/capture-progress.py
```

The script builds and runs each library rather than describing it, and reports
what came out. That report is the measurement. As captured, for a 200- to
400-step bar:

| generator | language | form on a pipe |
| --- | --- | --- |
| `schollz/progressbar` v3 | Go | **redraws** — 18,578 B, 298 CR, **no newline at all** |
| `tqdm` | Python | **redraws** — 1,415 B, 18 CR, one trailing newline |
| `curl` | C | **redraws** — header rows, then one rewritten summary row |
| `tqdm` nested | Python | **redraws**, plus `CSI A` between rows |
| `rich` | Python | one final line; consults `isatty` |
| `ora` | Node | first and last frame only; consults `isatty` |
| `indicatif` | Rust | silent |
| `vbauerster/mpb` v8 | Go | silent |
| `cli-progress` | Node | silent |

Most progress libraries go quiet on a pipe, which makes it tempting to assume
they all do. `schollz/progressbar` is why that assumption is not safe, and it is
the worst case of the set: no newline anywhere, so the entire run is one row and
every line-oriented stage sees exactly one line.

The silent libraries are kept as generators even though they produce no fixture.
A measurement that something is silent is only worth having if it is re-checked;
a release that starts emitting would otherwise go unnoticed.

A generator whose library, toolchain, or network is unavailable is reported and
skipped, and its fixture is left alone. A generator that exits non-zero is also
skipped rather than captured, so a stack trace can never be committed as though
it were a library's output.

## Invariants

- Filtering is a rendering step. It is not a substitute for retaining the raw capture, and callers
  are expected to keep one.
- Terminal rendering is faithful but not lossless: the overwritten frames are gone. It reports how
  many it absorbed so a caller can disclose that, and it is applied where the caller still holds the
  exact byte count and, in the shell tool, the untouched progress stream.
- Row rendering is bounded to one row's width, so a bar that redraws for an hour costs the width of
  its final frame rather than the length of the stream.
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

`tests/terminal.rs` and `tests/progress_collapse.rs` run against the captured fixtures. The former
also pins that rendering is independent of how the stream is chunked, which matters because a capture
ring receives reads that split rows, carriage returns, and escape sequences wherever the kernel filled
the buffer.
