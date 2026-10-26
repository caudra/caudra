---
name: skillpack-translate
description: Translate a vendored skill pack (pstack from cursor/plugins, mattpocock from mattpocock/skills) from its harness's vocabulary into Caudra's, so agents follow the pages Caudra serves as written. Use for the pstack-sync run, after `make sync-pstack` or `make sync-mattpocock` reports TRANSLATE, NEW, or ORPHAN, and for any edit under caudra-pstack/translated/ or caudra-mattpocock/translated/.
---

# Translating a skill pack into Caudra

Caudra bundles third-party skill packs, each as one builtin skill (`pstack`, `mattpocock`) that serves every vendored file as a page. Each pack is written for another harness: pstack for Cursor, mattpocock for Claude Code and its peers. The agent that reads a page must be able to follow it as written, with no mapping table. So every foreign tool, field, path, model slug, command, and feature is rewritten here, once, by us.

The rules shared by every pack are below. Each pack's own mapping is a reference page; read the one for the pack you translate first:

- [pstack](references/pstack.md): Cursor's tools, subagent fields, model roles, pstack's ported scripts, and benny.
- [mattpocock](references/mattpocock.md): Claude Code's tools, the issue tracker, grilling through `question`, and the methodology changes we made.

## Layout

`<crate>` is `caudra-pstack` or `caudra-mattpocock`, and `<pack>` is `pstack` or `mattpocock`.

- `<crate>/pack.json` says what `make sync-<pack>` copies: each upstream root's files and trees, where they land, and what is left out.
- `<crate>/upstream/` is the copy `make sync-<pack> ARGS='--source <checkout>'` writes. Never edit it by hand. It is the base for the next translation and the license record. `upstream.json` hashes every stored file, and under `sources` the raw upstream bytes of each Markdown file whose links the sync rewrote.
- `<crate>/translated/` has the same layout. A file there replaces its upstream page. Copy a file here only when it needs a change.
- `<crate>/translated/translations.json` records, for each translated file, the hash of the upstream file it was translated from. `make sync-<pack> ARGS='--mark <path>...'` writes it, with paths as in the overlay, such as `pstack/skills/how/SKILL.md` or `skills/tdd/SKILL.md`.

## The check

`cargo test -p <crate> --lib` fails when:

- a served page still names a foreign term (the list is in the crate's `library.rs` or `lib.rs` tests, and its allowlist names each exception and why),
- a translation's recorded upstream hash differs from the upstream file (the upstream file changed: retranslate),
- a translation has no upstream file (upstream deleted or moved it: delete or move the translation),
- a relative link leads nowhere,
- a page tells the user to type a bare `/<skill>` instead of `/<pack>:<skill>`.

## How to translate a file

1. Copy `<crate>/upstream/<path>` to `<crate>/translated/<path>`.
2. Rewrite what the pack's reference covers, plus the sentences whose reasoning depends on the other harness. Keep upstream's structure, headings, voice, and every rule that still holds. Do not reword text that is already right, and do not add commentary about the translation. A pack's reference may name deliberate changes of method beyond vocabulary; make only those.
3. Read the result as a Caudra agent would: every tool, field, value, path, and command must exist in Caudra and be spelled as its schema spells it.
4. Run `make sync-<pack> ARGS='--mark <path>'` and the check.

## How to retranslate after a sync

`make sync-<pack>` prints one line per file that needs work:

- `TRANSLATE <path>`: upstream changed a translated file. Diff the old and new upstream text (the command is printed with it, `git diff HEAD -- <crate>/upstream/<path>` before you commit), apply the same change to the translation, translating any new foreign text, then `--mark` it.
- `NEW <path>`: upstream added or changed a file that has no translation. Translate it when the check names it.
- `ORPHAN <path>`: the translation's upstream file is gone. Delete the translation, or move it with the upstream file and retranslate.
- `PORT <script>`: a ported script changed. Update its Rust port in `<crate>/src/scripts/` to match, with tests.

## Caudra's side

What every pack's reference maps onto:

| Caudra | Spelled |
| --- | --- |
| delegate work | the `task` tool, a task. Several `task` calls in one turn run in parallel. `mode: "plan"` is read-only; `mode: "build"` may edit. `background: true` returns at once, and the task's final report arrives in the conversation by itself: never poll, and check a running task with one `task_control` `status` call. A settled task is resumed by its `task_id`. |
| a task in its own worktree | the brief tells it to `git worktree add` a new branch under the scratch directory, off the base the step names, and to work only there |
| ask the user | the `question` tool. A headless run has none, so the step says to ask in plain text then. |
| a todo list | `todo_write` |
| run another skill of the pack | load `<pack>/<name>` with the `skill` tool and follow it. When the text tells the user to type it, write `/<pack>:<name>`. |
| write a skill | load `caudra-docs/skills` with the `skill` tool |
| past sessions | `history.search` and `history.read` in `code`: `scope="session"` for this chat, `scope="project"` for this project's chats |
| project instructions | `AGENTS.md` (Caudra also reads `CLAUDE.md`, so keep naming both where upstream does) |
| project skills | `.caudra/skills/`, `~/.config/caudra/skills/` |
| a temporary file | the scratch directory, which Caudra gives each project and writes to without asking: `$TMPDIR/...` in shell commands and env assignments, and in prose "the scratch directory (the `Scratch directory` path in the Environment block, `$TMPDIR` in shell commands)" on first mention. A config value nothing expands takes the placeholder `<scratch directory>/...`. Never a hard-coded `/tmp`. |
| keep going until X | `/goal X`, which the user sets (autopilot answers for them). Within a turn, keep working until X holds. |
| a forge | the command the step gives, such as `gh pr create`. Commands get the shell tool's environment, so `gh` needs its token or keyring variables forwarded with `/env` or `[agent.shell_env] pass`. |

A page's own links stay relative: a page is served at `<pack>/<path>`, and a relative link resolves against its directory.

Map first. Remove a step only when Caudra has nothing like it, and then remove it cleanly, with no "skipped" note. If a later step needs what the removed one made, rewrite that step too.

## Plan mode

Caudra opens sessions in Plan mode, where only the session plan changes. A step that writes a plan or a spec says: in Plan mode, write it with the `plan` tool (Caudra's session plan; Ctrl+O opens it, and autopilot can review it); in Build mode, write the file where the step names. A step that builds, edits, commits, or writes any other file needs Build mode: the user switches with Tab, and a task's mode is capped by its caller's, so `mode: "build"` cannot lift a task out of Plan mode.
