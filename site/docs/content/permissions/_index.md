+++
title = "Permissions"
weight = 6
[extra]
group = "Reference"
+++

# Permissions

Caudra reviews a tool's action before it sends the call to the tool. Reusable decisions bind to a host-generated authority, the tool implementation, validated input, typed resources, and execution type.

Permissions control consent. They do not sandbox shell commands or external MCP processes.

In a [remote workspace](/docs/remote-workspaces/#project-context-and-trust), local file grants do not cover remote resources. Remote project permission denies apply immediately, while allows require review of the exact fetched asset. Caudra approval cannot override the Workcell server's immutable policy.

## Resolution order

Caudra resolves a tool call in this order:

1. Plan-mode and executor restrictions reject prohibited operations.
2. A matching deny blocks the call.
3. A matching configured ask requires confirmation.
4. Stored and configured allows cover resources independently. Every unresolved resource needs coverage.
5. Builtin command-family asks apply when no stored or configured allow covers the command.
6. Builtin and trusted-plugin policy can allow known operations.
7. YOLO mode can skip an ask or prompt, but cannot override a deny.
8. The effective default allows, denies, or prompts.

Allows can combine across resources. A shell chain can use separate grants for `git diff *` and `git status *`. Exact-input rules still apply only to their original complete input.

## Lifetimes and authorities

Authority controls what a rule covers. Lifetime controls how long the rule remains active. The prompt selects them independently.

Exact call is the default authority. Trusted tool profiles can also offer a URL prefix, filesystem subtree, shell command pattern, shell workdir, search provider, or whole MCP tool. Caudra does not infer these choices from names in an external tool schema.

Prompt decisions use four lifetimes:

| Lifetime | Behavior |
|---|---|
| Once | Allows only the current bound invocation |
| Conversation | Survives resume and applies to subtasks in the same root conversation |
| Project | Applies in the same canonical project directory |
| Global | Applies in every project |

Project and global rules still bind to the exact native tool contract or MCP server authority and tool contract. Replacing a tool, changing an MCP endpoint, or changing an MCP schema invalidates the old authority.

A user-created fork starts with no conversation grants and no inherited explicit YOLO state. Subtasks share the root conversation's grants. `/new` also starts clean.

## Permission prompts

The prompt shows the action, risk, selected authority, and typed resources before its controls. Resources already covered by another rule appear after unresolved resources, marked with the scope that covers them and the authority that carries it, such as `already allowed · project · rg *`. The scope tells you whether the coverage outlives the session and the authority tells you how far it already reaches, which is what decides whether granting again changes anything. The body expands when the terminal has room and scrolls on smaller terminals while the controls remain visible.

The action line carries the arguments its own summary does not already name, in the compact form the transcript uses. Likely secret values and URL query values are masked there.

| Key | Action |
|---|---|
| `y` | Allow this exact call once |
| `Up` / `Down` | Select a host-generated reusable authority or a reviewed command |
| `Left` / `Right` | Walk the selected row along its range, where it has one |
| `<` / `>` | Narrow or widen every command row at once |
| `Enter` | Write your own command pattern for the selected command |
| `s` | Allow the selected authority for the conversation, after confirmation |
| `a` | Allow the selected authority for the project, after confirmation |
| `A` | Allow the selected authority globally, after confirmation |
| `n` | Add guidance and deny once |
| `d` | Deny this exact call for the project, after confirmation |
| `D` | Deny this exact call globally, after confirmation |
| `PageUp` / `PageDown` | Scroll the body when it does not fit |
| `Esc` or `Ctrl-C` | Deny once |

The footer names widening only when the selected authority has somewhere to go, and the page keys only when the body is taller than the space it has. It names the command-row keys only on a wide terminal, because a narrow footer gives its rows to the decision keys.

Reusable approvals are exact by default. A parsed shell command is scoped one command at a time, described below, ahead of the unrestricted workdir and global shell choices. Broad authorities require explicit selection. Unrestricted URL, search, shell, and MCP authorities also require a typed phrase. Each authority advertises its valid lifetimes. Whole-tool MCP authority is conversation-only.

A filesystem authority arrives as a ladder. Its narrowest rung covers the directories the request touched, and each step up covers the directory above, as far as the filesystem root. The rungs share one row, and `Left` and `Right` walk it, so widening changes the reach the row names rather than adding choices to scroll through. A rung reaching outside the repository is marked `outside repo`. A rung that takes in your home directory is marked `outside home` and needs the `ALLOW OUTSIDE HOME` phrase. Moving to another authority and back returns the ladder to its narrowest rung.

A URL authority is a ladder too. `Left` and `Right` walk it the same way, one path segment at a time, so a page can be scoped to the section it sits in. A request for `https://example.com/path/to/sub/page` starts at `https://example.com/path/to/sub/page/**` and widens through `https://example.com/path/to/**` and `https://example.com/path/**` to `https://example.com/**`, where the row reads `Any page on this origin`. A path deeper than eight segments offers its eight deepest prefixes and the origin. A URL with no path offers the origin alone. Reaching other origins is a separate authority that needs the `ALLOW ANY URL` phrase.

Multiple requests are queued by request ID. The prompt identifies the requesting subtask. A subtask request cannot replace a prompt from the main agent or another subtask. Confirming a reusable authority also approves every pending request it already covers. Conversation, project, and global lifetimes limit which pending conversations or projects can share that approval. Allow once and deny decisions resolve only the selected request.

## Per-command scopes

A shell call often runs several commands at once. Each reviewed command gets its own row in the prompt, above a separate `Or grant broadly instead` section holding the blanket authorities. A row starts on `this command`, which remembers that exact command in that workdir, and widens to a token-bound pattern such as `git status *`. Narrowing past the start reaches `this call only`, which remembers nothing for that command. Rows already covered start there, so answering without moving remembers only what was undecided.

The `Commands` heading counts the rows still waiting, such as `2 of 5 need approval`. The call is atomic, so those rows are the whole decision.

A pattern is offered whenever it would match the command, including commands whose operands the shell expands. `ls src/ src/*/` offers `ls *` and `wc -l a.rs b/*.rs` offers `wc *`, because the pattern only pins leading literals. Commands using command substitution get no pattern, because the reviewed text is not what would run.

`Left` and `Right` walk the selected row. `<` and `>` walk every row one step at a time, so the usual answer stays two keystrokes however many commands were batched. Both ends of a row clamp rather than wrap.

The scope you pick controls what Caudra remembers, never what runs. A shell call is atomic, so answering the prompt runs every command in it. Rows left on `this call only` are kept out of the stored rules. Confirming with `s`, `a`, or `A` writes one rule per row that chose a reusable scope, so `/permissions` lists each command separately and revoking one leaves the others in place. The lifetimes offered are the ones every granted row allows.

A row that adds nothing files nothing:

- When one row already covers another, the covered row reads `covered by \`rg *\`` and contributes no rule. A pipeline running `rg` twice writes one `rg *` rather than two. Narrowing the covering row hands the other row its own choice straight back. The covered row stays selectable, because `Enter` can still reach a wider pattern than the one covering it.
- A row whose existing coverage is at least as durable as the scope you are granting files nothing. Answering `a` on a row only a conversation rule covers still files, or the authority would disappear with the session. The builtin allowlist never counts as coverage here, because it is a default that a configured ask or deny overrides. Widening a covered row still files, since a wider pattern reaches commands this prompt is not about.

Rows replace the whole-request `This command in this workdir` and command-pattern choices, because a row on its narrowest reusable rung reproduces the first and a row at its widest reproduces the second.

### Writing your own pattern

`Enter` opens an editor on the selected command row. A pattern must end in `*`, hold at least one literal token before it, use at most eight tokens, and match the command on that row. Caudra refuses anything else and names the reason.

Two patterns are accepted with a caution:

- A pattern with one literal covers a whole program, such as `python *`. It is marked `[any invocation]` and needs the `ALLOW BROAD SHELL ACCESS` phrase. Typing a pattern Caudra offers on that row, such as `ls *`, is graded like the offered rung rather than as a broad grant.
- A pattern overlapping a builtin always-ask family, such as `git push *`, is marked `[always-ask family]`. It is allowed because those are often the commands worth shortcutting, but read it before confirming.

Grading is structural. It checks the shape of the pattern and that it matches the command, and it cannot know what a program does with its arguments. `sed -n *` grades clean, yet GNU `sed` can run shell commands through the `e` escape. Write patterns for programs whose arguments you understand.

## Plan mode

While plan mode is active, Caudra withholds the authority that would outlive the plan. Remembered project and global rules do not apply, allows from `permissions.toml` do not apply, and the prompt offers only the once and conversation lifetimes. Deny and ask rules still apply, because they only restrict access.

A conversation grant made while planning does apply for the rest of the plan. Approving broad shell authority once therefore lets the agent keep exploring with scripts and searches instead of asking about each command. The grant stays with the conversation after you leave plan mode.

## Stored rules

Use `/permissions` to inspect and revoke active conversation, project, and global rules. The picker distinguishes exact, selected-input, filesystem subtree, URL subtree, URL origin, and unrestricted authority. It also shows active legacy denies and builtin, configured, or trusted-plugin policy. Read-only policy must be changed at its source.

Project and global prompt decisions are stored in the `permission.rules` row of Caudra's owner-only SQLite state database. Exact input and resource values are stored as SHA-256 digests. Host-derived command patterns are stored as clear-text policy, such as `git diff *`. Review metadata keeps only anonymous field positions and value types. Selected-input authorities store their JSON pointers and a digest, but never the selected values.

Conversation rules are stored with the session. A persistent write must finish before Caudra executes the approved call. If storage fails, the durable approval fails and the prompt remains open in the TUI.

Ephemeral runs still read and write project and global permission decisions in the persistent state database. Conversation rules stay with the temporary session and disappear with it.

## TOML policy

Caudra reads policy from:

- Global: `~/.config/caudra/permissions.toml`
- Project: `.caudra/permissions.toml`

TOML supports `deny`, `ask`, and `allow`. Deny and ask rules are active from global and project config. Only validated shell allow patterns can grant authority. Global shell allows are active immediately and bind to Caudra's native Workcell shell contract. Other name-only allows remain inactive review candidates.

Project shell allows require trust before they become active. On startup, the TUI opens `/permissions` and offers to trust the project policy digest for the canonical project. The digest covers project shell allows and every project deny or ask rule. Editing any of them invalidates trust, and `/permissions` can revoke trust explicitly. Non-interactive modes leave untrusted project allows inactive. Project deny and ask rules remain active without trust because they only restrict access. Changing projects with `/cd` reloads the destination policy before further tool calls.

An unreadable or malformed permissions file fails closed. Caudra disables inherited allows and denies tool calls until the file is fixed.

A project `prompt` default cannot weaken a global `deny` default, including per-tool and MCP defaults.

`/permissions` lists inactive entries as `needs review`, shows trusted config policy, and can revoke remembered prompt decisions. Configured, builtin, and plugin policy appears read-only, because it is changed at its source.

```toml
default = "prompt"

[shell]
allow = [
    "rg *",
    "git status *",
    "git diff *",
    "git log *",
]
ask = [
    "*",
    "git commit *",
    "git push *",
    "git reset *",
]
deny = [
    "sudo *",
    "rm -rf *",
]

[mcp.github]
deny = ["admin_delete"]
```

Shell allow and ask patterns use literal tokens followed by an optional bare `*` token. The wildcard matches zero or more complete arguments. It must be separated by a space, so `git status *` is valid and `git status*` is rejected. `allow = true` is the all-command `*` pattern for native shell tools. Patterns contain at most eight tokens and 256 bytes. Literal tokens may contain ASCII letters, digits, `.`, `_`, `/`, `@`, `:`, `=`, `+`, and `-`.

Command patterns never authorize a redirect that names a file. Writing to a file, reading from a file, and heredocs all produce a protected request carrying the complete original command. File descriptor duplication such as `2>&1` names no file and stays an ordinary reviewable command. Path-qualified executables remain path-qualified, so `git status *` does not authorize `/tmp/git status`.

No configured allow reaches a protected command, whatever its pattern. A protected command that no deny names is left uncovered and prompts, so the reviewed text reaches you rather than a rule written against a shape it does not have.

Restrictive rules are also matched against the resolved executable name, so `/bin/rm -rf build` cannot dodge a `rm *` deny. Allows are matched against the reviewed text alone.

For shell allow and ask rules, the most specific matching pattern wins and ask wins a tie. Any matching deny still blocks the complete call. Rule order in the file has no effect. A catch-all `ask = ["*"]` can therefore coexist with more specific read-only allows.

Caudra also asks by default for these command families unless a configured or remembered allow covers them: `rm`, destructive Git operations, `chmod`, `chown`, `dd`, `mkfs`, network transfer and remote-login commands, and process termination commands.

Caudra allows `echo` by default when every argument is literal after quoting, so the reviewed text is exactly what the shell runs. Parameter expansion, command substitution, globs, tildes, braces, redirects, and operators all fall back to the normal prompt, and a configured ask or deny still overrides the default. Single quotes keep their contents literal, so `echo '$HOME'` is allowed while `echo $HOME` asks.

A configured scope matches glob-like, whatever its effect:

| Pattern | Matches |
|---|---|
| `*` or `**` | Any scope |
| `prefix*` | Values starting with the prefix |
| `cmd *` | Bare `cmd` or `cmd` followed by arguments |
| `dir/**` | The directory and descendants, using path components |
| Other | Exact text |

New remembered decisions use structured matching. Shell command authorities use the strict token pattern grammar above.

## Typed resources

Structured requests distinguish files, directories, URLs, commands, queries, and custom resources.

- File matching uses normalized path components and resolves existing symlinks.
- Protected paths such as `.git`, `.ssh`, `.aws`, and dotenv files require exact authority.
- Access outside the project prompts even when ordinary project reads and writes are trusted.
- URL matching and fetching reject credentials and ambiguous encoded path separators or dot segments.
- Shell authority includes the initial working directory.
- A deny that intersects any resource blocks the complete call.

File-write tools remain pre-allowed inside the project working directory. Read-only filesystem tools declare scopes and trusted native policy allows them by default. Loading a skill is allowed by default wherever the skill lives. A skill name is a catalog key rather than a path, so the tool can only open a `SKILL.md` under Caudra's own skill directories or read a skill built into the binary. Explicit deny rules can therefore block `file_read`, `file_glob`, `file_grep`, `file_index`, `skill`, or image access without adding normal prompt noise.

Every registered model tool reaches the permission manager. A tool without declared scopes receives its canonical validated input as an exact fallback scope. The `batch` container routes inner calls through the same manager. Native `python_execution` is isolated and has no inner tool calls.

## MCP tool calls

Generic MCP tools use the complete canonical JSON input as their exact authority.

Caudra binds approval to one immutable MCP transport, server configuration digest, remote tool name, and discovered tool contract. A reconnect cannot switch the transport after approval. A changed description or schema creates a different contract.

Generic field names such as `path` or `command` do not create reusable resource authority. External servers control their schemas, so Caudra treats these values as display information unless the host has a trusted typed profile.

The TUI can select broad whole-tool MCP authority for the current conversation. It requires the `ALLOW MCP TOOL` confirmation phrase and cannot be stored for a project or globally. ACP and SDK clients remain exact-only.

## Shell parsing

Bash scopes include the normalized initial working directory. Tree-sitter walks control flow, loops, and functions so each command in `&&`, `||`, `;`, and pipeline expressions is authorized independently. Analysis drops redirect operands from the reviewed text, so a command that redirects to or from a file keeps the complete original command as its protected authority.

The parser preserves executable directory prefixes for allow matching. `/usr/bin/git status --short` therefore does not inherit `git status *` authority. Deny and ask rules also check the normalized executable name, so `rm *` still restricts `/bin/rm`. Quotes keep argument boundaries, and a wildcard consumes complete arguments rather than arbitrary text.

A shell prompt offers a reusable pattern derived from the reviewed command. A curated table names the families whose first operand is data rather than a subcommand, so `rg needle src/` offers `rg *` and keeps the search term out of the rule. The table also names the families whose subcommand sits behind a namespace token, so `npm run build` offers `npm run build *`. Outside the table the leading lowercase words become the prefix, so `git commit -m "message"` offers `git commit *`.

A prefix never reaches past a flag, so `docker -H tcp://host run nginx` offers no pattern. A prefix taken from outside the table must name more than the executable and must leave at least one operand behind, which is why `git status` offers no pattern. Caudra also offers no pattern that shares a prefix with a default ask family, because storing `git checkout main *` would silence the `git checkout *` ask.

Command substitution, process substitution, subshells, arithmetic expansion, wrappers such as `eval` and `sudo`, file redirects, heredocs, and parse failures all mark the command protected. A protected command is reviewed as one whole command line.

Configured allows, scope allows, and command patterns never cover a protected command. The two unrestricted shell authorities do, because they already authorize any command the user can write, including `tee` and an interpreter reading a script from standard input. Selecting one requires the `ALLOW BROAD SHELL ACCESS` phrase. Deny rules still apply. Builtin command-family asks do not reach protected commands, so a broad grant also silences those asks for them.

Caudra executes the reviewed command text unchanged. Workcell may reduce completed shell output before the model receives it. The TUI shows raw output while the command runs, then switches to a labelled filtered view that the user can toggle back to raw.

The initial working directory is context, not confinement. An approved shell command can still access files, the network, and inherited environment variables.

## Plugin rules

Bundled plugins can declare trusted host policy for resources they own. Builtin allows apply only to implementations marked as bundled by the loader. User plugins do not inherit native or bundled trust. Global user plugins need a valid `plugin.toml` before they can register allow policy. Project plugins can register deny rules only. Remembered Lua decisions bind to the plugin name, tool name, entry source, required Lua modules, description, and schema. A reload during review cannot switch the approved handler generation.

Lua plugin API capabilities remain separate. `plugin.toml` controls whether plugin code may call filesystem, network, process, and environment APIs. Tool-call permissions control whether the agent may invoke a registered tool.

## YOLO mode

`/yolo` and `--yolo` skip prompts after deny rules and hard restrictions have run. The status bar shows `[yolo]` while enabled, or `[!]` when the terminal is too narrow to spell it. The warning is the last thing the bar drops, so it stays visible after the token counts and the reasoning level are gone.

An explicit `/yolo` choice is stored with the root conversation. A user-created fork and `/new` start without that explicit state. `--yolo` supplies the initial default for a fresh root.
