---
title: "Permissions"
description: "What runs freely, what asks first, TOML rules."
---

Caudra reviews a tool's action before it sends the call to the tool. Reusable decisions bind to a host-generated authority, the tool implementation, validated input, typed resources, and execution type.

Permissions control consent. They do not sandbox shell commands or external MCP processes.

In a [remote workspace](/docs/remote-workspaces/#project-context-and-trust), local file grants do not cover remote resources. Remote project permission denies apply immediately, while allows require review of the exact fetched asset. A remote call that can change the workspace, such as a shell command or a file write, can be remembered only as `this call`. Caudra approval cannot override the Workcell server's immutable policy.

## Resolution order

Caudra resolves a tool call in this order:

1. Plan-mode and executor restrictions reject prohibited operations.
2. A matching deny blocks the call.
3. A winning configured ask requires confirmation, using the shell specificity rules below.
4. Stored and configured allows cover resources independently. Every unresolved resource needs coverage.
5. Builtin command-family asks apply when no stored or configured allow covers the command.
6. Builtin and trusted-plugin policy can allow known operations.
7. Auto mode can skip an unmatched default prompt. YOLO mode can skip an ask or prompt. Neither overrides a deny rule.
8. The effective default allows, denies, or prompts.

Allows can combine across resources. A shell chain can use separate grants for `git diff *` and `git status *`. Exact-input rules still apply only to their original complete input.

## Lifetimes and authorities

Authority controls what a rule covers. Lifetime controls how long the rule remains active. The prompt and permission manager select them independently.

The default authority is narrow: an exact call, exact paths for eligible reads, or an exact shell command in its reviewed working directory. Trusted tool profiles can also offer names-only browsing, a filesystem subtree, URL prefix, command pattern, shell workdir, search provider, or whole MCP tool. Caudra does not infer these choices from names in an external tool schema.

Prompt decisions use four lifetimes:

| Lifetime | Behavior |
|---|---|
| Once | Allows only the current bound invocation |
| Conversation | Survives resume and applies to subtasks in the same root conversation |
| Project | Applies in the same canonical project directory |
| Global | Applies in every project |

Project and global rules bind to the native tool contract or MCP server authority and tool contract. Explicit filesystem family grants cover the trusted native tools described under [Typed resources](#typed-resources). Replacing a tool, changing an MCP endpoint, or changing an MCP schema does not silently transfer authority.

A user-created fork starts with no conversation grants and no inherited explicit permission mode. Subtasks share the root conversation's grants and mode. `/new` also starts clean.

## Cross-session messages

[Cross-session messaging](/docs/messaging/) needs the global experimental opt-in in every participating process. Tool permission rules cannot enable it. `send_message` and `publish_message` are side effects subject to outgoing authorization. Rules can treat them differently, because one publication can reach many sessions and wake each of them. ReadOnly sessions cannot send or publish. Plan sessions ask before either unless [YOLO mode](#yolo-mode) is on.

Receiving has a separate policy under `[agent.messaging]`:

| `inbound` | Behavior |
|---|---|
| `auto` (default) | Automatic delivery only between Ask-mode sessions with matching Build or Plan mode and the same canonical local working directory. Other cases are held, including every message from a [script](/docs/messaging/#messages-from-scripts) |
| `accept` | Automatic delivery from any local session or script, subject to [rate limits](/docs/messaging/#rate-limits-and-cost). Messages can start billable turns |
| `hold` | Requires local approval before delivery |
| `refuse` | Rejects incoming messages |

Project configuration may only tighten the effective policy, in the order `accept < auto < hold < refuse`. A user-controlled session setting can replace a global default, including choosing `accept` over `auto`, but cannot relax an explicit project restriction. `/messages` provides review and session policy controls. Approving held content does not approve tool actions it requests, and manual approval cannot override project refusal.

The policy treats direct messages, topic messages, and broadcasts alike. Subscriptions decide which publications reach a session at all, and the policy then decides whether each one waits for review. Rate limits are the only volume control. Two sessions that deliver to each other automatically can keep starting billable turns until you press Esc in either one. See [rate limits and cost](/docs/messaging/#rate-limits-and-cost).

The same policy limits what `read_topic` returns from the [message history](/docs/messaging/#message-history). Under `auto`, stored messages from senders outside the automatic cohort are counted without their text. Under `hold` or `refuse`, the agent cannot read the history. Reading is free of side effects, so it needs no outgoing authorization.

A [consumer group](/docs/messaging/#consumer-groups) member takes only work whose message its policy would deliver automatically. Under `auto` it leaves work from scripts, other workspaces, and other modes to members with `accept`, and under `hold` or `refuse` it takes none. `/groups` counts the queued work the policy skips. `work_assignment` reports on the session's own work. Its `list` action only reads. `complete`, `retry`, and `fail` change shared work state, so ReadOnly sessions cannot report, and Plan sessions ask first unless [YOLO mode](#yolo-mode) is on. No tool creates, joins, or changes a group.

The history keeps every message as plain text, direct messages included. File permissions keep other users out, but any program running as your user can read it, for example with [`caudra message log`](/docs/cli/#caudra-message).

The peer manager separates browsing from approval. Open one message's review before deciding. Its review expires when the session controls change. The policy view names this session as its scope and shows project restrictions. Applying a less restrictive policy requires confirmation because held messages may become eligible for automatic delivery. Refuse rejects new arrivals and leaves existing held messages available for inspection or rejection.

Messaging assumes you trust other programs running as the same operating-system user. Local user checks do not prove that a peer is a genuine Caudra process. Any such program can run `caudra message` under any `--from` label, so a script label identifies a sender only by its own claim. Peer text reaches the model attributed to its sender and remains untrusted input. The agent is asked to treat it as a request from that sender, within its own mode and permissions and below your instructions. Slash commands, attachment syntax, and claims of approval inside it are literal text. They grant no authority, but malicious text can still influence a model.

Delivery exposes the message to the recipient's model provider as conversation context. The recipient can have a different provider, permission mode, or saved grants. Even two Ask-mode sessions need not have equivalent authority. Do not use another session to route around a denied action. Use `hold` or `refuse` when you need manual control or isolation rather than the automatic policy's trust heuristic.

## Permission prompts

A prompt asks one question, such as `Allow shell command?` or `Allow fetching a web page?`, and offers numbered answers. Above them it shows the request as the tool receives it. Likely secret values and URL query values are masked, and terminal controls are escaped. Commands are coloured as shell code, with the body of a heredoc in the language of the program that reads it, as described under [Markdown](/docs/markdown/#shell-code). The folder a command starts in appears when it is not the project root, and a path outside the project is marked `⚠ Outside this project`. The right edge of the title names the subagent that asks, the place in the queue, such as `1 of 3`, and how many commands of a batch need an answer.

```text
╭ Allow shell command? ────────────────────────────────────────────────────────╮
│                                                                              │
│  cargo test -p caudra-agent permissions::structured                          │
│                                                                              │
│  ❯ 1. Yes                                                                    │
│    2. Yes, and allow ‹cargo test *› for this conversation                    │
│    3. Yes, and always allow ‹cargo test *› in this project                   │
│    4. No, and tell the agent what to do instead                              │
│                                                                              │
│  ← broader  → narrower  e customize  ? details  Esc no                       │
╰──────────────────────────────────────────────────────────────────────────────╯
```

The phrase between `‹` and `›` is the scope a remembered answer covers. A command offers `this exact command`, any suggested templates such as `cargo check -p <value>`, and its token prefixes from the longest to the shortest, such as `cargo test -p caudra-agent *`, `cargo test *`, and `cargo *`. It starts on a suggested template, else on the [derived prefix](#shell-parsing), else on the exact command. A web page starts on `this page and below`, and a file read on `this file`. `Left` broadens the scope and `Right` narrows it before you answer, and the answers change with it. Blanket grants such as `any shell command` are offered only in [Customize](#customize).

A scope that is a template [learned from earlier commands](#suggested-patterns) adds a muted line under the answers that remember it, such as `Learned from 4 similar commands. <value> is caudra-agent or caudra-ui.` The count includes the command being asked about. In a batch the line describes the focused command, and it disappears when `Left` or `Right` moves the scope off the template.

Warnings appear above the answers as `⚠` lines. They name a protected path, a line Caudra cannot check command by command, the reach of the selected scope, or a [decision engine](#decision-engine-advice) caution such as `⚠ May delete files (88%)`. A muted line explains an unusual reason for asking, such as `Plan mode asks before anything it can't prove read-only.` or a line starting with `Auto asked:`. Details gives the usual reason.

The answers depend on what can be remembered:

| Request | Answers |
|---|---|
| Most requests | `Yes`, `Yes, and allow ‹scope› for this conversation`, `Yes, and always allow ‹scope› in this project`, `No, and tell the agent what to do instead` |
| A broad scope while planning, or no project to bind a rule to | The same without the project answer, renumbered |
| A line Caudra cannot check command by command, such as an inline script | `Yes, run it once`, `No, and tell the agent what to do instead` |

The `No` answer opens a one-line field for guidance. `Enter` sends the guidance to the agent, and an empty field denies without it. `Esc` leaves the field without answering.

| Key | Action |
|---|---|
| `1` to `4` | Choose that answer |
| `y` / `s` / `a` / `n` | Yes, allow for this conversation, allow in this project, or No with guidance. A letter does nothing when its answer is not offered |
| `Up` / `Down`, `k` / `j` | Move the highlight |
| `Enter` | Choose the highlighted answer |
| `Left` / `Right` | Broaden or narrow the scope of the focused command |
| `Tab` / `Shift-Tab` | Focus the next or previous new command of a [batch](#per-command-scopes) |
| `<` / `>` | Broaden or narrow every new command of a batch |
| `e` | Open [Customize](#customize), or the [step-through](#step-through) when several commands need an answer |
| `?` | Open or close [Details](#details) |
| `PageUp` / `PageDown` | Scroll a prompt that does not fit |
| `Esc` | Deny from the answers or Details. Elsewhere, go back without answering |
| `Ctrl-C` | Deny |

Answers and footer keys also take mouse clicks. A click counts when the press and the release land on the same answer. An answer needs a key press made after the prompt was drawn, so a held or repeated key cannot answer a prompt you have not seen. Terminals that report key releases rearm on release. On older terminals, a key that would carry over into the next decision makes the footer read `Press Tab, then press the key again.`

For eligible reads, the scope starts on `this file` or `these files`. This remembers the resource rather than the full input, so reading the same file with another `offset` or `limit` does not need a new grant. Exact-path search grants still constrain the search expression. The exact-call authority continues to require the complete original input. Protected paths, remote requests, plan restrictions, and unavailable persistence can limit the offered scopes and lifetimes.

A filesystem scope is a ladder. Its narrowest rung covers the directories the request touched, and each step up covers the directory above, as far as the filesystem root. `Left` steps up to the directory above and `Right` steps back down, so widening changes the reach the answers name rather than adding answers to scroll through. A rung reaching outside the repository adds `⚠ Outside this repository`. A rung that takes in your home directory adds a red `⚠ Outside your home directory` and needs [confirmation](#confirming-broad-grants).

A URL scope is a ladder too, walked one path segment at a time, so a page can be scoped to the section it sits in. A request for `https://example.com/path/to/sub/page` starts on `this page and below`, which covers `https://example.com/path/to/sub/page/**`. `Left` widens it through `pages under example.com/path/to/sub/` and `pages under example.com/path/` to `any page on example.com`, and `Right` walks back toward the page. The scheme is shown only when it is not `https`. A path deeper than eight segments offers its eight deepest prefixes and the origin. A URL with no path offers the origin alone. `any public web page` reaches other origins, is offered only in Customize, and needs [confirmation](#confirming-broad-grants).

Multiple requests are queued by request ID. A subtask request cannot replace a prompt from the main agent or another subtask. `Yes` resolves only the selected request.

When policy changes, each pending request rechecks its own current policy and refreshes partial coverage. Separate grants for A and B can cumulatively settle a request needing both. Automatic settlement requires complete coverage under the current deny, ask, lifetime, and workspace restrictions. Policy is checked again before execution, so stale displayed coverage cannot authorize a call. Conversation, project, and global lifetimes limit which waiting requests can share approval.

### Customize

`e` opens Customize in place of the answers, with every scope and lifetime the request offers. On a batch, `e` opens the [step-through](#step-through) instead, and Customize opens from its Review page. `Esc` returns to where Customize was opened without answering.

```text
╭ Allow shell command? · Customize ────────────────────────────────────────────╮
│                                                                              │
│  cargo test -p caudra-agent permissions::structured                          │
│                                                                              │
│  Effect     [Allow]  Deny                                                    │
│  Remember    Once  [This conversation]  This project   All projects          │
│  Scope        this exact command                                             │
│               cargo test -p caudra-agent *                                   │
│             ❯ cargo test *                                                   │
│               cargo *  ⚠ broad                                               │
│               your own pattern…                                              │
│               any command in this folder  ⚠ broad                            │
│               any shell command  ⚠ broad                                     │
│                                                                              │
│  Tab next field  ↑↓ choose  Enter apply  v advanced  Esc back                │
╰──────────────────────────────────────────────────────────────────────────────╯
```

| Field | Choices |
|---|---|
| Effect | Allow, or Deny to refuse the request once, for this project, or for all projects |
| Remember | Once, This conversation, This project, or All projects, limited to the lifetimes the chosen scope allows |
| Scope | The ladder from the main view, then `your own pattern…` and the blanket grants. A suggested template is marked `suggested` with how often it was seen, and while highlighted it adds a line saying what each slot stands for. A scope that needs [confirmation](#confirming-broad-grants) is marked `⚠ broad` |

Opened from the step-through's Review, Customize lists scopes for the whole line: these commands exactly and the blanket grants. Under Deny it offers `this exact script`.

While planning, Remember offers This project only for a scope that is not broad, and never All projects. A broad scope adds the line `While planning, broad scopes last for this conversation.` See [Plan mode](#plan-mode).

| Key | Customize action |
|---|---|
| `Tab` / `Shift-Tab` | Focus the next or previous field |
| `Up` / `Down` | Choose a scope |
| `Left` / `Right` | Switch the effect while Effect is focused, otherwise change how long |
| `Enter` | Apply, or open the field for [your own pattern](#writing-your-own-pattern) |
| `i` | Open the [argument pattern inspector](#argument-pattern-inspector) on a template |
| `v` | Show or hide the full rule the chosen scope would store |
| `?` | Open [Details](#details) |
| `Esc` | Go back without answering |

### Confirming broad grants

Some scopes reach far enough that Caudra asks again before it stores them. Choosing one adds a red line naming what the agent could then do, such as `⚠ The agent could run any shell command without asking.` For this conversation, press `Enter` again to confirm. A project or global rule needs its phrase typed exactly:

| Phrase | Scope |
|---|---|
| `ALLOW BROAD SHELL ACCESS` | Any shell command, any command in a folder, a whole program such as `cargo *`, or a pattern of your own with one literal word |
| `ALLOW OUTSIDE HOME` | A folder that takes in your home directory |
| `ALLOW DIRECTORY CHANGES` | Changes anywhere below a folder |
| `ALLOW FILE CHANGES` | Changes to the files of this request |
| `ALLOW ANY URL` | Any public web page |
| `ALLOW ANY SEARCH` | Any search query |

A whole MCP tool can be allowed for this conversation only, so it needs the second `Enter`. Scopes that reach protected files, writes outside the project, and widened [argument patterns](#argument-pattern-inspector) also need the second `Enter`. When several remembered commands need confirmation, the strongest one applies, and phrases are asked in command order. `Esc` goes back without answering.

### Details

`?` opens Details over the prompt, and `?` again closes it. `Esc` in Details denies the request. Details lists plain sections, each shown only when it has something to say:

| Section | Content |
|---|---|
| What will run | The full command, or what the tool asks for |
| Where | The folder or workspace it applies to |
| Why Caudra is asking | The reason, and the `Auto asked:` line when Auto left it to you |
| Already allowed | Each covered command and the rule that covers it |
| What choices 2 and 3 allow | What each remembering answer would store, scope by scope |
| Decision engine | Every [caution](#decision-engine-advice) with its likelihood |
| Tool | The tool and where it comes from, such as `shell (built-in)` |
| Input | The input as the tool receives it, with likely secrets masked |

A value that Caudra stores only as a fingerprint reads `a fixed value Caudra can't show`.

## Per-command scopes

A shell line can run several commands. When it does, the prompt gives each command its own row, in the order the line runs them, and the title counts the new ones:

```text
╭ Allow shell commands? ─────────────────────────────────────────── 3 of 6 new ╮
│                                                                              │
│  cargo fmt -p caudra-agent && cargo clippy -p caudra-agent --tests && rm     │
│    -rf target/tmp && git push origin HEAD && rg -n TODO src && head          │
│                                                                              │
│  ▸ new      cargo fmt -p caudra-agent           ‹cargo fmt *›                │
│    new      cargo clippy -p caudra-agent --te…  ‹cargo clippy *›             │
│    new      rm -rf target/tmp                   ‹this exact command›         │
│    asks     git push origin HEAD                git push * · config          │
│    allowed  rg -n TODO src                      read-only                    │
│    allowed  head                                read-only                    │
│                                                                              │
│  ❯ 1. Yes                                                                    │
│    2. Yes, and allow these 3 commands for this conversation                  │
│    3. Yes, and always allow these 3 commands in this project                 │
│    4. No, and tell the agent what to do instead                              │
│                                                                              │
│  Tab next  ← broader  → narrower  <> all  e one by one  ? details  Esc no    │
╰──────────────────────────────────────────────────────────────────────────────╯
```

| Status | Meaning |
|---|---|
| `new` | Needs an answer. The row shows the scope a remembering answer stores for it |
| `asks` | An ask rule covers it, so it asks every time and cannot be remembered. The row names the rule and its source |
| `allowed` | Already covered. The row names what covers it, such as `read-only`, `built-in`, `rg * · project`, or `this conversation` |

More than three allowed rows fold into one line, such as `+ 5 already allowed`, and Details lists them.

`▸` marks the focused row. `Tab` and `Shift-Tab` move between the new rows. `Left` moves the focused row one step broader and `Right` one step narrower, and `<` and `>` do the same for every new row. A row at the end of its ladder stays put. Each ladder runs narrowest first: `this time only`, `this exact command`, suggested templates, then token prefixes from the longest to the shortest. The shortest prefix names the program alone, such as `cargo *`, and needs [confirmation](#confirming-broad-grants). A row starts on its suggested template, else on its derived prefix, else on the exact command.

Answers 2 and 3 count the rows they remember, as in `these 3 commands`, or name the scope when only one row is remembered. With every row on `this time only`, they are left out. Answer 2 remembers each row for this conversation, and answer 3 in this project. The [step-through](#step-through) gives each command its own lifetime.

The scope you pick controls what Caudra remembers, never which commands run. `this time only` runs the command now without remembering it. Shell operators still decide execution order and conditional execution, and the effects of earlier commands stay when a later one fails. Each remembered row becomes its own rule, so `/permissions` lists the commands separately and revoking one leaves the others in place.

A chosen scope can cover another row. Caudra then stores only the covering rule, as long as it lasts at least as long as the other row. Rules you already have never replace a scope you chose. They change what the rows show and leave authorization to the policy check.

Remembered rows are validated together and stored in one transaction, so a failed write leaves no partial set. An [ephemeral session](/docs/sessions/#ephemeral-sessions) keeps its conversation in a separate database. Its project and global rules are stored first, and a failed conversation write withdraws them before the prompt reopens.

A line Caudra cannot check command by command, such as one running an inline script, keeps its rows and offers only `Yes, run it once` and `No`. [Shell parsing](#shell-parsing) lists what makes a line uncheckable.

### Step-through

`e` on a batch opens the step-through at the focused row. It has a page for each new command and a final Review page. The tabs at the top name each command by its executable and subcommand, and a visited page gets `✓`.

```text
╭ Allow shell commands? ─────────────────────────────────────────── 3 of 6 new ╮
│                                                                              │
│   cargo fmt │ cargo clippy │ rm │ Review                                     │
│                                                                              │
│  cargo fmt -p caudra-agent                                                   │
│                                                                              │
│    1. This time only                                                         │
│    2. This exact command                                                     │
│  ❯ 3. cargo fmt *                                                            │
│    4. cargo *                                                        ⚠ broad │
│    5. Your own pattern…                                                      │
│                                                                              │
│  Remember for ‹this conversation›                                            │
│                                                                              │
│  ↑↓ scope  ←→ how long  Enter choose  Tab next  Esc back                     │
╰──────────────────────────────────────────────────────────────────────────────╯
```

A page lists the command's whole ladder as numbered answers, with badges such as `suggested` and `⚠ broad`. `Your own pattern…` opens a field for [a pattern of your own](#writing-your-own-pattern). `Remember for` sets how long this command is remembered: this conversation, this project, or all projects. It offers only the lifetimes the chosen scope allows and is hidden while `This time only` is chosen. In plan mode it offers this conversation, and this project for a scope that is not broad.

| Key | Page action |
|---|---|
| `Up` / `Down` | Move the highlight |
| `Enter` or a number | Choose that scope and go to the next page |
| `Left` / `Right` | Change how long this command is remembered |
| `Tab` / `Shift-Tab` | Go to the next or previous page without choosing |
| `i` | Open the [argument pattern inspector](#argument-pattern-inspector) on a template |
| `Esc` | Discard the draft and return to the main view |

Review lists every command with its scope and lifetime. Asking and allowed commands say so.

```text
╭ Allow shell commands? ─────────────────────────────────────────── 3 of 6 new ╮
│                                                                              │
│   cargo fmt │ cargo clippy │ rm │ Review                                     │
│                                                                              │
│  cargo fmt -p caudra-agent       cargo fmt *             this project        │
│  cargo clippy -p caudra-agent …  cargo clippy *          this conversation   │
│  rm -rf target/tmp               this time only                              │
│  git push origin HEAD            asks every time         git push * · config │
│  rg -n TODO src                  already allowed         read-only           │
│  head                            already allowed         read-only           │
│                                                                              │
│  ❯ 1. Yes, and remember as listed                                            │
│    2. No, and tell the agent what to do instead                              │
│    3. More options for the whole script…                                     │
│                                                                              │
│  ↑↓ choose  Enter confirm  Shift-Tab back  Esc main view                     │
╰──────────────────────────────────────────────────────────────────────────────╯
```

`Yes, and remember as listed` (`y`) stores each command for its own lifetime, and reads `Yes, run them once` when nothing would be remembered. `No` (`n`) asks for guidance as in the main view. `More options for the whole script…` opens [Customize](#customize) with scopes for the whole line. `Shift-Tab` returns to the last page, and `Esc` to the main view.

Only Review applies the draft. A policy change while the step-through is open keeps your choices, unless it withdraws a scope you chose. That row then falls back to its default, and the next answer needs a fresh key press.

### Argument pattern inspector

A suggested template such as `cargo check -p <value>` can be inspected and edited before it is stored. Press `i` on it in Customize or on a step-through page. The inspector shows fixed command words, variable argument positions, working directory, evidence, and whether the current command matches. Patterns match parsed static arguments. They never interpolate captured values into command text.

Use `Tab` to focus controls, then arrows to select a slot or mode. The mode shortcuts are:

| Key | Mode | Coverage |
|---|---|---|
| `1` | Values | Selected observed literal values |
| `2` | Exact | One exact literal value |
| `3` | Glob | One entire argument matched by a glob |
| `4` | Regex | One entire argument matched by a regular expression |
| `5` | Any | Any one literal argument. Values that look like options stay refused |

`e` edits a constraint, `o` shows observed values, and `c` switches between observed tuples and independent combinations when available. `N` edits the pattern name and `n` edits a slot label. Names are display labels and do not change matching or rule identity. The executable, fixed arguments, argument count, and slot positions stay fixed.

A suggested slot is named after the long flag before it, such as `<package>` after `--package`. A slot whose observed values all contain `/` is `<path>`, and any other is `<value>`. Repeated names are numbered, as in `cp <path1> <path2>`. A name is only a hint. The main view, Customize, Discover, and the Rules pane add a line stating what each slot stands for, such as `<value> is caudra-agent or caudra-ui.` It names at most four values and counts the rest.

For example, `cargo check -p <value>` can restrict its variable argument to the observed values `caudra-agent` and `caudra-ui`. It does not cover another executable, another subcommand, or extra arguments. Option-looking values remain rejected unless the host has proved the position is data. Choosing Any keeps that check.

Slots with an unknown role can include arguments after fixed flags. That position does not prove the argument is data or establish the flag's arity. These values can select program operations, so the inspector and approval review caution against widening them.

Suggestions start with observed literal values and observed tuples, without wildcards. Tuples preserve combinations. If two slots were seen as `(alpha, json)` and `(beta, text)`, they allow only those pairs. Independent combinations also allow `(alpha, text)` and `(beta, json)`. Widening one slot to Glob, Regex, or Any still leaves the tuple restriction in force until you explicitly choose independence.

Glob matching is case-sensitive over UTF-8 bytes. `?` matches one byte, `*` excludes a literal `/`, and `**` can cross `/` in globset's recursive forms. Backslash escapes metacharacters, and braces and character classes use globset syntax. For example, `src/*.rs` matches `src/main.rs` but not `src/nested/mod.rs`, while `src/**/*.rs` can match both. These patterns match one complete argument without shell or filesystem expansion. The inspector displays these semantics beside the preview.

Regex uses Rust's finite-automata regular-expression engine, with whole-argument matching. Regular constructs such as alternation, groups, and repetition work. PCRE look-around and backreferences do not. Expressions, nesting, compiled size, input, and retained evidence are bounded. Invalid or oversized expressions show a compile error and cannot be used.

`p` takes a valid pattern back to the page or to Customize without answering, and `Esc` leaves the inspector without it. A pattern that does not match the current command must be fixed or the command run once instead. Policy rechecks the complete call on approval. Glob, Regex, Any, and independent multi-slot combinations need a [second `Enter`](#confirming-broad-grants) before they are stored. A remembered pattern is explicit execution authority, even for an unfamiliar CLI. It never makes that command read-only or sandboxed.

### Writing your own pattern

To remember a token prefix of your own, choose `your own pattern…` in Customize or `Your own pattern…` on a step-through page. A prefix must end in `*`, hold at least one literal token before it, use at most eight tokens, and match the command. The line under the field says whether it matches or names the reason it is refused. `Enter` accepts a matching pattern and `Esc` leaves the field. Caudra validates the text without running it.

Two patterns are accepted with a caution:

- A pattern with one literal covers a whole program, such as `python *`. It reads `⚠ Any use of this program.` and needs the same [confirmation](#confirming-broad-grants) as `any shell command`. Typing a pattern Caudra offers for that command, such as `ls *`, is graded like the offered scope rather than as a broad grant.
- A pattern overlapping a builtin always-ask family, such as `git push *`, reads `⚠ Overlaps commands Caudra always asks about.` It is allowed because those are often the commands worth shortcutting, so read it before confirming.

Grading is structural. It checks the shape of the pattern and that it matches the command, and it cannot know what a program does with its arguments. `sed -n *` grades clean, yet GNU `sed` can run shell commands through the `e` escape. Write patterns for programs whose arguments you understand.

## Suggested patterns

Caudra learns permission suggestions from tool use. Suggestions are never automatic grants or the default future scope. Live recognition needs at least three eligible command observations and can propose a pattern within one session. A compound request can supply several observations. Counts do not establish successful execution.

Only calls that are allowed keep teaching. A call refused or cancelled before it runs is withdrawn from live recognition, and a waiting prompt loses any template it supported. Saved conversations mark each call that permissions refused, including each refused command of a batch, and history discovery skips those calls. Conversations saved by earlier versions carry no marks, so their refused calls still count.

Automatic suggestions come only from eligible live observations or imported session history. A command without enough evidence has no suggested argument template. You can also [create a template from explicit command text](#command-templates) without history. The executable, workdir, and argument structure stay bound, and every template requires explicit approval.

Recognition compares individual commands with fixed argument structure and observed literal values. It can handle unfamiliar CLI names without a read-only executable allowlist. Payload and sensitivity guards still exclude interpreted code, unsafe expressions, and suspicious literals. It does not learn whole command sequences or generate scripts. Basic chains and pipelines are analyzed per command only where control flow and working-directory context can be established.

The local TUI loads bounded historical proposals in the background at startup, for the current tab, and after `/cd`. Loading is cached per canonical project and cancellable, and stale results are discarded. Remote and ephemeral sessions do not run history discovery. Discover marks imported history as unverified, and the argument pattern inspector also names its unknown outcomes and historical execution context. Analysis assumes standard Bash startup and uses the session's current stored cwd as an approximation. Loading these proposals does not make them verified live evidence.

Open `/permissions discover` for the Discover tab. The Rules tab contains stored rules and policy. Discover lists only proposals, which are not active permissions. Click either tab or press `Ctrl-G` to switch. Opening Discover does not force a new scan.

| Key | Discovery action |
|---|---|
| `Enter` | Focus the selected proposal's pane |
| `Ctrl-E` | Create permission from the selected proposal |
| `Ctrl-G` | Switch between Rules and Discover |
| `Ctrl-O` | Show the discovery overview and scan diagnostics |
| `Ctrl-R` | Scan or refresh, bypassing an older cached result. An active scan is not restarted |
| `Ctrl-X` | Cancel the current scan request |

The overview reports `Not scanned`, `Loading`, `Ready`, `Partial`, `Unavailable`, or `Cancelled`. Completed scans say in plain sentences how many sessions, rows, and tool calls were read, with sizes such as `24.0 KiB of 4.0 MiB`, and how many suggestions were found. `Partial` lists cutoffs and exclusions, including per-parent row limits, omitted command scopes, and source obligations. `Unavailable` gives the reason discovery cannot run. Cancellation discards late results for that request and leaves active permissions unchanged. Press `Ctrl-R` to retry.

Imported proposals need at least two observations from two independent parent sessions. Repeated commands in one session are insufficient. An empty list can also reflect unsupported or sensitive commands, sample limits, or dismissed and snoozed definitions.

Each proposal row leads with its template, such as `cargo test -p <value>`, and ends with how often it was seen and where, such as `4× · 2 conversations`. Proposals from imported history count sessions instead of conversations. The pane beside the list shows the template, what each slot stands for, the commands it covers with their counts, what still asks, and where it was seen. Its last line says the proposal is not active and that `Ctrl-E` drafts a permission to review. `Enter` focuses the pane without granting anything, and `PageUp` / `PageDown` and the mouse wheel scroll it.

Create permission opens an editable draft. Select the currently registered local shell target and supply concrete command text and an absolute workdir for fresh host analysis. Historical tool identity and context do not authorize the new rule. Configure its input constraints and lifetime, then review and Save as described under [Stored rules](#stored-rules). A matching live prompt is not required. Opening the draft or analyzing its source grants nothing.

`Ctrl-D` dismisses the selected definition for the project, and `Ctrl-S` snoozes it for 24 hours. These preferences persist as bounded project and definition digests and leave active rules unchanged. Renaming a display label does not change the definition's identity.

Use [`caudra permissions discover`](/docs/cli/#discovering-patterns-from-history) for an explicit read-only scan and its limits. Neither background discovery nor the CLI executes historical commands.

## Plan mode

Selecting Plan while Build work runs does not change that work. A conflicting submission offers a choice to keep editing, queue in Plan, or stop the work first. See [Selecting Plan while work runs](/docs/queue/#selecting-plan-while-work-runs).

The [`plan` tool](/docs/tools/#plan) works only on the session's own plan. The main agent can replace it in Plan and Build, and tasks can only read it. Exact-target writes can receive scoped plan approval in either mode, but explicit denies, configured asks, and a default Deny still apply. Reading the plan needs no approval in any mode, local or remote, and a default Deny does not block it. An explicit deny rule still refuses the read, and an explicit ask rule or a forced prompt still asks. A profile can disable the tool.

A plan prompt asks `Allow reading the plan?` or `Allow changing the plan?`. It shows the path of a local plan and names a remote plan `this session's plan`.

While plan mode is active, Caudra withholds the authority that would outlive the plan. Remembered project and global rules do not apply, and allows from `permissions.toml` do not apply. Deny and ask rules still apply, because they only restrict access.

A conversation grant made while planning does apply for the rest of the plan. Approving broad shell authority for the conversation lets the agent keep exploring with scripts and searches instead of asking about each command. The grant stays with the conversation after you leave plan mode. `Yes` covers only the current call.

A narrow scope, such as this exact command or `git log *`, can also be kept for this project. Caudra stores the project rule together with a copy for this conversation, so the grant covers the rest of the plan as well as later Build work, and `/permissions` lists both. A plan in another conversation still asks, because project rules never apply while planning. A broad scope, such as `cargo *` or `any shell command`, lasts this conversation at most, and no grant made while planning lasts for all projects.

With [YOLO mode](#yolo-mode) on, plan mode skips these prompts too. YOLO approves each call once and stores no rule. Plan mode still refuses what it always refuses, such as file writes outside the plan.

Reading this project's plans and memory notes never asks, in plan mode or any other mode. The file and code tools may read the `plans/` and `memories/` directories under `…/state/caudra/projects/<project-id>/` (see [Directory layout](/docs/configuration/#directory-layout)). The allowance covers reads only, and another project's documents still ask. A symlink inside those directories cannot carry a read outside them. Tools in a remote workspace run on another machine, so they get no such allowance.

Listing and reading notes with the `memory` tool needs no approval either, local or remote. Writing or deleting a note goes through the normal checks.

## Read-only agents

A read-only agent is a subagent that may not change anything: research tasks, `task` calls in plan mode, and every agent a workflow starts in `read-only` capability mode. Only audited host tools with safe call effects are eligible. Ordinary MCP and plugin tools are excluded. When an audited `shell` is available, Caudra judges each command line rather than the tool as a whole.

A line is admitted when the classifier rules every command in it read-only and every path it touches resolves inside the project. `git diff`, `git log`, `rg`, and `wc -l` pass. Git options that only shape what is printed pass in their `=` form, such as `--format='%h %ci'`, `--date=short`, and `--since=2.weeks`. A format holding `%G` does not, because signature placeholders run gpg. Anything that writes, anything the parser could not read, and anything reaching outside the project is refused with a message naming the call and the reason, and the agent can retry with a narrower line. The same classifier answers here and in plan mode, so a command plan mode would refuse is refused here too.

The confinement check resolves symlinks before it answers, so a link checked into the repository cannot carry a read out of the project. It says nothing about what an admitted command reads inside the project: a read-only agent can still read any file you have.

A glob that [shell parsing](#shell-parsing) can check, such as `src/*`, is expanded the way Bash does by default before the check: `*` never crosses `/` and skips names that start with a dot. Every match must resolve inside the project. A glob counts only where its matches are paths to read: an operand of `ls`, `cat`, `head`, `tail`, `wc`, `du`, `file`, `stat`, `tree`, or `sort`, an operand after the pattern of `grep` or `rg`, or a path after `--` in a Git read, so `echo src/*` is not admitted. Expansion stops after 4096 directory entries or 1024 matches, and a glob past either limit is refused. A remote workspace refuses globs, because Caudra cannot list its files.

A read-only agent's file tools can also read this project's plans and memory notes without asking, as described under [Plan mode](#plan-mode). A shell line reading them is refused, because they sit outside the project. A task can read the session plan with the `plan` tool but cannot replace it. An agent a workflow starts while the session is in Plan can read it too. One started in Build gets no plan.

Profiles cannot relax this restriction. Safe lazy tools remain discoverable through `tool_search`, but loading a schema cannot grant write access or the main agent's right to replace the plan.

## Stored rules

Use `/permissions` to manage stored conversation, project, and global rules and inspect policy. Builtin, configured, and trusted-plugin policy appears alongside stored rules. Each row reads as a sentence: the effect, the scope, how long it lasts, and who added it. Commands and command patterns in the list and the detail pane are coloured as shell code.

```text
Allow  cargo test *              this project       you
Deny   git status --short        this conversation  you
Ask    bash: git push *          always             config
Allow  bash: git log *           always             built-in
```

A stored rule lasts for `this conversation`, `this project`, or `all projects`, and policy reads `always`. `other project`, `inactive`, and `revoked` mark rules that do not apply here. The origin is `you`, `config`, `plugin`, or `built-in`.

The detail pane summarizes the selected rule in plain words: what it allows or refuses, where, for how long, and who added it. `Enter` focuses the pane without editing or revoking anything. The full scope, with named inputs, typed targets, context, and authority constraints, appears in Edit. Project-config trust rows open a separate confirmation.

| Key | Manager action |
|---|---|
| `Ctrl-N` | New rule |
| `Ctrl-E` | Edit the selected stored rule, or Edit source when a verified local source is available |
| `Ctrl-U` | Duplicate the selected stored rule |
| `Ctrl-Y` | Copy the selected stored rule, leaving its source unchanged |
| `Ctrl-K` | Start a separate Revoke review |
| `Ctrl-F` | Cycle All, Here, Other, and History filters. History shows revoked records |

The toolbar also supports mouse input. Wide terminals show the list and details side by side. Narrow terminals show the focused pane. `Tab` / `Shift-Tab` switches focus, and arrows or `PageUp` / `PageDown` navigate the focused pane. The mouse wheel scrolls the pane under the pointer.

Edit replaces the stored rule and retires its previous record when saved. Duplicate creates another rule without retiring the source. If a lifetime change crosses separate conversation and persistent databases, use explicit Copy instead of replacement. Copy and a later Revoke are separate reviewed writes, not an atomic move. Copy leaves every source rule active, including deny and ask rules.

### Editing saved authority

The editor has Rule, Targets, Arguments, Template, Changes, Scope, and Test sections. Use `Tab` or arrows to focus controls and `Enter` or Space to activate them. `Enter` while editing a field accepts that field only.

In Rule, choose a registered target, effect, lifetime, and project binding. Capability families are available only where offered by the target. Effects are Allow, Deny, and Ask. Saved lifetimes are Conversation, Project, and Global. Once belongs to a live prompt. Only Project lifetime takes a project binding, either the current project or an explicit absolute path. Workdir and template context are separate constraints. Changing lifetime or project binding does not rewrite them.

Authoring uses the current host catalog, not tool names or arbitrary identity text. The implemented provider supports audited local Workcell registrations, subject to the current tool filter, agent mode, and host availability. Remote, MCP, and plugin tool authority authoring is unavailable. Existing active rules remain inspectable and revocable. Label-only edits preserve their authority. Revoked records are inspection-only.

Targets offers only the selected registration's resource kinds, access types, conditions, and match modes:

| Resource | Offered match modes |
|---|---|
| File or directory | Exact path, filesystem subtree, Any |
| Shell command | Exact command, token prefix, command template, Any |
| URL | Exact URL, URL origin, URL subtree, Any |
| Search query | Exact query, Any |
| Isolated Python or environment-inspection custom resource | Exact value, Any |

Shell targets can have an exact, subtree, or Any workdir condition. Directory targets offer an exact recursion condition. Glob and Regex are command-slot modes, not general resource match modes. A rule matches when any of its targets does, and a target matches when all of its access, protection, and attribute conditions hold. Removing the last target leaves an invalid blank. Unrestricted resources and wildcard conditions require explicit choices.

Arguments constrains the whole tool input independently of targets. Choose exact JSON input, selected JSON pointers, or explicitly unconstrained input. Missing input is distinct from JSON `null`. Changing a target does not remove an existing input constraint. Supply replacement input or explicitly choose Keep old input pin when retaining that constraint is intended.

A value Caudra stores only as a fingerprint reads `a fixed value Caudra can't show`, and sanitized review labels do not recover it. Such a constraint stays in force until you choose a replacement mode and enter a value. Label-only changes can keep it unchanged. New rules, copies, and authority expansions require reviewable values. A display label cannot substitute for a missing target or input.

Choose Preview (`Ctrl-P`), inspect the before/after Changes and resulting Scope, and confirm the listed authority changes when required. Save (`Ctrl-S`) is a separate action and waits for durable acknowledgment. Background validation never saves. Draft or context changes invalidate the reviewed preview and require fresh review. Saving a grant or relaxing a restriction can release pending requests after policy rechecks.

### Command templates

To create a template without a live request or historical proposal:

1. Choose New and select the registered local shell target. In Targets, add a Command target and configure its access and protection conditions.
2. Open Template. Enter a concrete Source for analysis, an absolute Analysis workdir, and a template name. Choose Create template.
3. The host derives the argument list, roles, and execution context through nonexecuting analysis. The initial template has only fixed literal arguments and no slots.
4. Select an eligible data or unknown-role argument and use Add/unlink slot to make it variable. Configure the remaining Rule and Arguments fields, then Preview, review, and Save.

Analysis requires one complete, static, reviewable shell command. It checks current binding and eligibility without executing the source. Creating a template does not add historical observations or prove that the command is safe.

Typed slot controls edit labels, allowed values, Exact, Glob, Regex, Any, and tuple or independent combinations. Their matching rules are described in the [argument pattern inspector](#argument-pattern-inspector). Manually added values and tuples are constraints, not observed evidence. Link to selected slot requires equal source arguments. Removing a slot requires an explicit fixed replacement for every occurrence. Adding, unlinking, linking, or removing slots requires fresh host analysis before the structural change is applied.

Discover activation and duplicated or copied templates also require fresh source analysis against the current registration. Historical context is not reused as current execution authority. Lifetime remains independent of the template's workdir and project context.

### Testing a draft

After a valid preview, open Test, enter example JSON input for the registered target, and choose Test, never execute. Analysis uses the current session binding and accepts a workdir in input only when the host supports it. No sample command or tool call is executed.

Matches this rule reports whether the draft covers that example. Effective policy separately reports Allowed by policy, Prompt, or Denied with the draft included. Other rules and execution restrictions can change that outcome. Allowed by policy is not a promise of execution. Dispatch gates are checked again when a real call runs.

### Editing policy sources

Edit source opens a verified local policy file or loaded plugin entrypoint in the local workbench. The host checks its path and loaded content before opening it. This edits local text, even from a remote workspace. Saving the file does not reload policy or grant fresh project-config trust. Reload and trust review are separate steps.

Verified-source editing currently requires Unix. Saves check the retained file identity and contents and refuse changed sources or symlink substitutions. Existing dirty remote tabs remain separate and are restored when you return.

Builtin policy, remote policy, and entries without a verified local source or supported editing API remain read-only in the manager. A navigable local plugin entrypoint enables text editing, not typed plugin authority authoring. Editing source policy never creates a stored override.

### Storage and recovery

Project and global prompt decisions are stored in the `permission.rules` row of Caudra's owner-only SQLite state database. Authorization uses SHA-256 digests for exact input and resource constraints. Selected-input authorities use JSON pointers and a digest. Host-derived command patterns are clear-text policy, such as `git diff *`.

Argument patterns and names-only browse grants use new durable selector or capability tags. Older binaries cannot open permission state containing those grants. There is no backward-compatibility layer. Update every Caudra process sharing the database before using these scopes, including sessions in other projects.

The permission manager also upgrades the database schema to fence delayed session saves. The upgrade requires other database users to close and creates a backup before changing the schema. It preserves existing rules without rebinding their authority. Older binaries cannot reopen the upgraded database.

Separate typed review metadata stores meaningful sanitized paths, command patterns, and recognized first-party input names and values. Likely secrets are redacted, bulk content and unrecognized fields are omitted, and descriptions are bounded. These descriptions can contain private project information even after sanitization. The database is owner-only, not encrypted. Review provenance is approved, recovered, or unavailable. Review metadata never grants authority or relaxes a stored constraint. Displayed omitted fields remain constrained for exact-input rules.

Conversation rules are stored with the session. A persistent write must finish before Caudra executes the approved call. If storage fails, the durable approval fails and the prompt remains open in the TUI.

Older review metadata requires the explicit [`caudra permissions repair-review`](/docs/cli/#repairing-review-descriptions) command. Its default is a dry run. Applying it repairs descriptions across persistent rules and stored conversations, takes a SQLite backup, and leaves authorization unchanged. Unverified historical values stay unavailable. Use `--retry-unavailable` to retry unavailable or incomplete recovered reviews while preserving approved reviews and existing verified labels.

Use `--database` to select an existing canonical absolute `caudra.db` path, and check the database path in the report before applying. Development binaries default to the separate `caudra-debug` namespace. The [CLI reference](/docs/cli/#caudra-permissions) shows how to target the production database explicitly.

Ephemeral runs still read and write project and global permission decisions in the persistent state database. Conversation rules stay with the temporary session and disappear with it.

For rules bound to another project or a renamed directory, use the read-only [CLI inventory and reviewed rebind](/docs/cli/#caudra-permissions). Authorization never follows an old stored project's current symlink to transfer authority. Rebinding is fresh authorization for the destination. Unknown hashes need verified candidates, and unsupported input constraints need fresh grants. Related deny or ask rules can block a transfer and remain active at the source when copied. Project-config trust and YOLO are never transferred.

Use [`caudra permissions audit`](/docs/cli/#caudra-permissions) for a bounded, read-only summary of logged prompts, decisions, and paired waits. Counts measure events, not unique requests or a prompt rate. Wait summaries exclude duplicate or colliding pairing keys. The CLI reference lists sampling limits and timestamp filtering.

## TOML policy

Caudra reads policy from:

- Global: `~/.config/caudra/permissions.toml`
- Project: `.caudra/permissions.toml`

TOML supports `deny`, `ask`, and `allow`. Deny and ask rules are active from global and project config. Only validated shell allow patterns can grant authority. Global shell allows are active immediately and bind to Caudra's native Workcell shell contract. Other name-only allows remain inactive review candidates.

Project shell allows require trust before they become active. On startup, the TUI opens `/permissions` and offers to trust the project policy digest for the canonical project. The digest covers project shell allows and every project deny or ask rule. Editing any of them invalidates trust, and `/permissions` can revoke trust explicitly. Non-interactive modes leave untrusted project allows inactive. Project deny and ask rules remain active without trust because they only restrict access. Changing projects with `/cd` reloads the destination policy before further tool calls.

A checkout of a git repository also accepts trust granted at the same place in a linked worktree or other checkout of that repository, as long as the digest is identical. The grant itself stays with the checkout where you made it. Revoking trust in any checkout revokes it at the same place in all of them. Project MCP servers and project workflows follow the same rule. See [Trust across checkouts](/docs/worktrees/#trust-across-checkouts).

An unreadable or malformed permissions file fails closed. Caudra disables inherited allows and denies tool calls until the file is fixed.

The file can start with `version = 1`, and a file without it counts as version 1. A version newer than this build reads fails closed the same way, so an older Caudra never applies rules it would misread. See [Config file versions](/docs/configuration/#config-file-versions).

A project `prompt` default cannot weaken a global `deny` default, including per-tool and MCP defaults.

`/permissions` lists inactive entries as `needs review` and shows trusted config policy. Source-backed policy is separate from stored rules. Use [Edit source](#editing-policy-sources) when a verified local source is available, then reload and review any required trust separately.

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

<!-- caudra-docgen:permissions-keys -->

#### Top level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default` | string | `prompt` | What a call that no rule matches does: `allow`, `deny`, or `prompt`. `allow` acts as `prompt` and waits in /permissions for review, and a project cannot weaken a global `deny` |

#### `[TOOL]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `allow` | bool \| string[] | unset | Scopes the tool may use without asking, such as `["git status *"]`, or `true` for every call. Only shell allows grant access, and a project shell allow waits until you trust the project policy. Other allows wait in /permissions for review |
| `ask` | bool \| string[] | unset | Scopes that always ask, such as `["git push *"]`, or `true` for every call |
| `deny` | bool \| string[] | unset | Scopes the tool may never use, such as `["rm -rf *"]`, or `true` for every call. A deny in either file blocks the whole call |
| `default` | string | unset | What a call of this tool that no rule matches does: `allow`, `deny`, or `prompt`. Unset follows the top-level `default` |

#### `[mcp.SERVER]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `allow` | bool \| string \| string[] | unset | Tools to allow without asking: a list of names, one name, `"*"` for every tool, or `true` for every tool. MCP allows wait in /permissions for review |
| `ask` | bool \| string \| string[] | unset | Tools that always ask, in the same forms as `allow` |
| `deny` | bool \| string \| string[] | unset | Tools the model may never call, in the same forms as `allow`. `false` in any of the three adds nothing |
| `default` | string | unset | What a call to a tool of this server that no rule matches does: `allow`, `deny`, or `prompt`. Unset follows the top-level `default` |

`caudra config example permissions` prints every `permissions.toml` key with its default, all commented out. [Reference configs](/docs/reference-configs/#permissions-toml) shows the same text.

<!-- /caudra-docgen:permissions-keys -->

Shell allow and ask patterns use literal tokens followed by an optional bare `*` token. The wildcard matches zero or more complete arguments. It must be separated by a space, so `git status *` is valid and `git status*` is rejected. `allow = true` is the all-command `*` pattern for native shell tools. Patterns contain at most eight tokens and 256 bytes. Literal tokens may contain ASCII letters, digits, `.`, `_`, `/`, `@`, `:`, `=`, `+`, and `-`.

Token prefix patterns do not authorize redirects to ordinary files or heredocs. These produce a protected request carrying the complete original command. Literal `/dev/null` redirects and file descriptor duplication such as `2>&1` can remain reviewable when their effects are understood. Argument patterns exclude all redirects. Path-qualified executables remain path-qualified, so `git status *` does not authorize `/tmp/git status`.

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

New remembered decisions use structured matching. Prefix authorities use the strict token grammar above. Argument patterns use the separate inspector constraints and are not TOML shell patterns.

Configured filesystem exact and subtree scopes expand only a leading `~` or `~/` on the rule side. Relative paths resolve against the explicit permission project base, then normalize with existing symlinks. Embedded `~` and `~user` stay literal. Generic raw-prefix scopes such as `prefix*` keep their text-prefix semantics. Tool arguments and shell text are unchanged. If a required home directory or path cannot be resolved, policy fails closed.

## Typed resources

Structured requests distinguish files, directories, URLs, commands, queries, and custom resources.

- File matching uses normalized path components and resolves existing symlinks.
- Protected paths such as `.git`, `.ssh`, `.aws`, and dotenv files require exact authority.
- Access outside the project prompts even when ordinary project reads and writes are trusted, apart from the scratch directory.
- URL matching and fetching reject credentials and ambiguous encoded path separators or dot segments.
- Shell authority includes the initial working directory.
- A deny that intersects any resource blocks the complete call.

File-write tools remain pre-allowed inside the project working directory and anywhere under the [scratch root](/docs/configuration/#directory-layout), which holds the directory `TMPDIR` points at. Approval covers the root rather than one project's subdirectory inside it, so `/cd` cannot strand a path the model was already given. Paths beside the root under the shared temp root still prompt. Read-only filesystem tools declare scopes and trusted native policy allows them by default. Loading a skill is allowed by default wherever the skill lives. A skill name is a catalog key rather than a path, so the tool can only open a `SKILL.md` under Caudra's own skill directories or read a skill built into the binary. Explicit deny rules can therefore block `file_read`, `file_glob`, `file_grep`, `file_index`, `skill`, or image access without adding normal prompt noise.

Every registered model tool reaches the permission manager. A tool without declared scopes receives its canonical validated input as an exact fallback scope. The `batch` container routes inner calls through the same manager. Native `python_execution` is isolated and has no inner tool calls.

### Directory browsing and content reads

Names-only browsing has its own `FilesystemBrowse` authority:

| Tool request | Access |
|---|---|
| `file_read` on a directory | `List`, direct entries in that directory |
| `file_glob` | Recursive filename enumeration under its root |
| `file_read` on a file | File content read, outside the browse family |
| `file_grep`, `file_index`, code-graph tools | Content search or analysis, outside the browse family |

`List names in this directory` covers only the exact directory's direct entries. `Browse names below this directory` explicitly permits recursive filename enumeration, including `file_glob` with other filename patterns. It also covers directory listings below the approved root. Neither choice permits reading file contents, content search, writes, or shell execution. Protected paths and symlink boundaries remain checked.

Directory `file_read` requests bind the canonical target and directory kind during preparation. Execution uses the prepared directory-listing operation and refuses a later file/directory kind change. A listing approval cannot become a content read through that race.

The broader `These directories and descendants` option uses `FilesystemRead`, covering reading, listing, and searching across the trusted native read family. It requires explicit selection and excludes writes and shell execution. Choosing Project binds it to the current project even if the directory lies outside it. A sibling checkout does not receive a directory grant by default.

## MCP tool calls

Generic MCP tools use the complete canonical JSON input as their exact authority.

Caudra binds approval to one immutable MCP transport, server configuration digest, remote tool name, and discovered tool contract. A reconnect cannot switch the transport after approval. A changed description or schema creates a different contract.

Generic field names such as `path` or `command` do not create reusable resource authority. External servers control their schemas, so Caudra treats these values as display information unless the host has a trusted typed profile.

In [Customize](#customize), the TUI can allow a whole MCP tool with any arguments for the current conversation. It needs a [second `Enter`](#confirming-broad-grants) and cannot be stored for a project or globally. ACP and SDK clients remain exact-only.

## Shell parsing

Bash analysis starts from the normalized initial working directory and tracks the possible directory at each command. Basic `&&`, `||`, `;`, and pipeline expressions can receive per-command scopes when control flow and context are proven. Reusable argument patterns require one known effective workdir. Unknown or ambiguous context falls back to manual exact-call review instead of assuming a directory or learning a sequence template.

The parser preserves executable directory prefixes for allow matching. `/usr/bin/git status --short` therefore does not inherit `git status *` authority. Deny and ask rules also check the normalized executable name, so `rm *` still restricts `/bin/rm`. Quotes keep argument boundaries, and a wildcard consumes complete arguments rather than arbitrary text.

A shell prompt can offer a token prefix derived from the reviewed command. A curated table names the families whose first operand is data rather than a subcommand, so `rg needle src/` offers `rg *` and keeps the search term out of the rule. The table also names the families whose subcommand sits behind a namespace token, so `npm run build` offers `npm run build *`. Outside the table the leading lowercase words become the prefix, so `git commit -m "message"` offers `git commit *`. This prefix heuristic is separate from generic argument-pattern recognition.

A derived prefix never reaches past a flag, so `docker -H tcp://host run nginx` offers no prefix. A prefix taken from outside the table must name more than the executable and must leave at least one operand behind, which is why `git status` offers no prefix. Caudra also avoids deriving prefixes that overlap a default ask family, because storing `git checkout main *` would silence the `git checkout *` ask.

A command's ladder also offers its other token prefixes, from the longest to the shortest, though a row never starts on one. Unlike the derived prefix, they can include flags. A flag keeps its value when the value is not a flag and holds no `/` or `.`, so `rustfmt --edition 2024 --check f.rs` offers `rustfmt --edition 2024 --check *`, `rustfmt --edition 2024 *`, and `rustfmt *`, and starts on the exact command. The shortest prefix names the program alone and is broad. It needs [confirmation](#confirming-broad-grants) and lasts this conversation at most while planning. It is never offered for shells, interpreters, wrappers, or privileged and indirect commands such as `bash`, `python3`, `env`, `sudo`, and `eval`, because any use of those can run any code. Prefixes that overlap a default ask family are left out, so `git log -1` offers `git log -1 *` and `git log *` but not `git *`.

Some lines cannot be checked command by command. Caudra then reviews the whole line as one protected request, names the cause in a `⚠` line, and offers only `Yes, run it once` and `No`:

| Cause | Examples | Warning | Auto |
|---|---|---|---|
| Loops and conditions | `for`, `while`, `if`, `case`, functions | `Uses loops or conditions that rules can't check` | Screened |
| File redirects | `> out.txt`, or data in a heredoc such as `cat <<'EOF'` | `Redirects input or output in ways rules can't check` | Screened |
| Words built at run time | `$(…)`, `<(…)`, `$((…))`, `$HOME`, assignments, `cd` to a computed path, globs such as `*.rs` or `~/x*` | `Builds part of the command only when it runs` | Screened |
| Wrappers | `env`, `time`, `xargs`, `command`, `builtin`, `coproc`, `bash script.sh` | `Runs a command through another program` | Screened |
| Inline scripts | A heredoc or here-string fed to an interpreter, `python3 -c`, `node -e`, `bash -c` | `Runs inline Python that Caudra can't check`, naming the language | Screened |
| Indirect code | `eval`, `source`, `.` | `Runs code through eval or source` | Always asks |
| Privilege | `sudo`, `su`, `doas` | `Runs with elevated privileges` | Always asks |
| Unreadable | Syntax errors, an executable that is not literal, other unsupported syntax | `Caudra couldn't read this command line` | Always asks |

The most severe cause names the line, so `eval` in a loop always asks. A redirect to `/dev/null` or between descriptors, such as `2>&1`, keeps a line checkable. Caudra looks inside loops, conditions, and substitutions for the commands they run. It also parses literal shell code given to `bash -c` or fed to a shell on standard input, up to two shells deep. Code that is not literal, or nested deeper, is unreadable. [Auto mode](#auto-mode) describes screening.

A glob stays checkable when it starts with a letter, digit, `.`, `_`, `/`, `+`, `@`, `:`, `,`, or `=`, and holds nothing but those characters, `-`, the wildcards `*` and `?`, and bracket classes such as `[0-9]`. `ls src/*` then gets a row and a ladder like any other command, a rule for `ls *` covers it, and deny and ask rules see it too. Any other glob is built at run time. A leading wildcard could expand to a name that reads as a flag, so write `./*.rs` rather than `*.rs`.

Interpreted payloads and sensitivity guards also prevent argument-pattern suggestions. Successful parsing alone does not establish reusable authority.

Configured allows, scope allows, and command patterns never cover a protected command. The two unrestricted shell authorities do, because they already authorize any command the user can write, including `tee` and an interpreter reading a script from standard input. Selecting one needs [confirmation](#confirming-broad-grants). Deny rules still apply. Builtin command-family asks do not reach protected commands, so a broad grant also silences those asks for them.

Routine probes count as read-only, so they do not ask. A name lookup qualifies when every name is literal, as in `command -v rg`, `type -t cargo`, or `which jq`. A version check qualifies when its only argument is one of these:

| Executables | Version argument |
|---|---|
| `python` | `--version`, `-V` |
| `node`, `npm`, `pnpm`, `yarn`, `perl` | `--version`, `-v` |
| `cargo`, `rustc`, `rustup` | `--version`, `-V` |
| `java` | `-version`, `--version` |
| `go` | `version` |
| `deno`, `bun`, `ruby`, `gcc`, `clang`, `make`, `cmake`, `git`, `just`, `rg`, `jq`, `uv`, `pip`, `pip3`, `nix`, `docker` | `--version` |

Versioned names such as `python3.12` count as their interpreter. `python -v` starts a verbose interpreter and `ruby -v` reads a program from standard input, so neither is listed. A path-qualified executable such as `/usr/bin/python3 --version` still asks.

Caudra executes the reviewed command text unchanged. Workcell may reduce completed shell output before the model receives it. The TUI shows raw output while the command runs, then switches to a labelled filtered view that the user can toggle back to raw.

On Unix, Workcell launches a host-bound absolute Bash executable with `--noprofile --norc -c`. It does not implicitly load login files or shell startup hooks. The existing environment allowlist still forwards `PATH`, proxy settings, and selected basic variables. This startup contract does not isolate the commands or their own configuration. See [Shell host configuration](/docs/cli/#shell-host-configuration) for executable selection.

The initial working directory is context, not confinement. An approved shell command can still access files, the network, and inherited environment variables.

The builtin read-only classifier is conservative about mutation flags, executable paths, expansions, and named file operands. For example, `git branch -D`, tag creation, and reflog expiration do not receive read-only authority. A path-qualified executable such as `./cat` does not inherit the builtin reader allowance, and an explicitly named protected operand such as `.env` requires review. These checks do not guarantee containment of recursive reads, program configuration, or repository code. Build and test commands are not read-only exemptions.

`execution_environment` still requires explicit approval under normal prompting policy. Its fixed probes can invoke sudo policy hooks or refresh credentials. A remembered exact grant can avoid repeated prompts without treating the tool as a pure read. Plan mode asks before running it as well, and [Plan mode](#plan-mode) describes which grants apply there.

## Plugin rules

Plugin rules apply only while Lua plugins run, which needs `experimental.lua_plugins`. See [Experimental features](/docs/configuration/#experimental-features).

Bundled plugins can declare trusted host policy for resources they own. Builtin allows apply only to implementations marked as bundled by the loader. User plugins do not inherit native or bundled trust. Global user plugins need a valid `plugin.toml` before they can register allow policy. Project plugins can register deny rules only. Remembered Lua decisions bind to the plugin name, tool name, entry source, required Lua modules, description, and schema. A reload during review cannot switch the approved handler generation.

Lua plugin API capabilities remain separate. `plugin.toml` controls whether plugin code may call filesystem, network, process, and environment APIs. Tool-call permissions control whether the agent may invoke a registered tool.

## Auto mode

Auto mode is experimental and belongs to the decision engine. Turn it on with `decision_engine = true` under `[experimental]` in the global `caudra.toml`, then restart Caudra. See [Experimental features](/docs/configuration/#experimental-features).

`/auto` toggles between Ask and Auto. `--auto` selects Auto at startup. The status bar shows `[auto]`, or `[a]` in a narrow terminal. Clicking the chip returns to Ask. `--auto` and `--yolo` are mutually exclusive.

Auto skips prompts only when no rule covers the call and the tool's default is Prompt. Configured asks, builtin asks, protected paths, and forced prompts still require approval. In plan mode, shell calls that deterministic checks cannot prove read-only still prompt. Deny rules and default Deny remain effective.

A shell line that cannot be [checked command by command](#shell-parsing), such as an inline script, needs more. Auto runs it only when a decision engine with `auto_screening = "enforce"` has read the whole line and flagged nothing, and policy did not change meanwhile. Indirect code, privilege, and unreadable lines always ask.

When Auto leaves a call to you, a muted line in the prompt says why:

| Line | Reason |
|---|---|
| `Auto asked: scripts need a decision engine to screen them` | The line needs screening and `auto_screening` is `off` |
| `Auto asked: the decision engine only advises, so it can't approve scripts` | The line needs screening and `auto_screening` is `shadow` |
| `Auto asked: the decision engine flagged this` | Enforced screening flagged the call |
| `Auto asked: the decision engine couldn't check this` | The engine failed or timed out |
| `Auto asked: this project turned off decision engine screening` | Project configuration disabled globally required screening |
| `Auto asked: sudo, su, and doas always need your approval` | The line runs with elevated privileges |
| `Auto asked: eval and source always need your approval` | The line runs indirect code |
| `Auto asked: Caudra couldn't read this command line` | The line could not be parsed |
| `Auto asked: protected files and commands always need your approval` | The call touches a protected path or command |
| `Auto asked: a rule says to ask first` | A configured or builtin ask rule matched |
| `Auto asked: plan mode needs your approval` | Plan mode withholds Auto |
| `Auto asked: this tool always asks for approval` | The tool or call forces a prompt |
| `Auto asked: Auto only decides for tools that prompt by default` | The tool's default is not Prompt |

An explicit mode choice is saved with the root conversation. A command-line mode overrides the restored choice. Global `always_auto = true` supplies the default when neither exists. Global YOLO settings take precedence if both defaults are enabled. Projects cannot set `always_auto` or `always_yolo`.

While the switch is off, `/auto`, `--auto`, SDK `--permission-mode auto`, and an SDK `set_permission_mode` request for `auto` fail with an error, and the mode does not change. With `always_auto = true`, sessions start in Ask and Caudra shows a notice. A session saved in Auto also starts in Ask. The saved choice is kept, so turning the switch on later brings Auto back. Ask, Plan, and YOLO are unaffected.

With the switch on, Auto works without a configured decision endpoint. With enforced engine screening, a flagged call, engine error, or timeout takes the ordinary prompt path. Without a channel for answering that prompt, the call is denied. Disabling globally required screening in project configuration also restores prompting for eligible calls.

Auto is not a security boundary. An engine miss can let an eligible call run. Engine predictions never grant read-only access or weaken deterministic permission rules.

### Decision engine advice

The decision engine needs the same `experimental.decision_engine` switch, and so do `caudra decisions` and `/decisions`. While the switch is off, `[decisions]` settings have no effect, and existing decision logs and shell duration history stay untouched. Turning the switch on enables nothing by itself. The engine still needs a base URL and a mode for each feature, and Auto still has to be selected.

Decision features are opt-in and configured in the `[decisions]` table of the global `caudra.toml`. A project may disable a feature or logging and shorten retention, but cannot change the base URL or thresholds, or enable a feature. A non-loopback base URL requires `allow_remote = true` because decision context leaves the machine. See the [configuration reference](/docs/configuration/#decisions) for fields, defaults, and supported modes.

Decision requests connect directly to the configured endpoint. They ignore ambient proxy variables and do not follow HTTP redirects, so project environment settings cannot redirect a loopback request.

HTTPS is required except for numeric loopback HTTP. A user-global `allow_http = true` permits non-loopback HTTP only together with `allow_remote = true`. Use this opt-in only when you control the transport protection, such as an encrypted tunnel. Private and CGNAT addresses do not prove that a tunnel exists. Projects cannot set either opt-in, and Workcell transport rules remain unchanged.

```toml
[decisions]
base_url = "http://127.0.0.1:8000"
timeout_ms = 800
log = false

[decisions.features]
permission_advice = "shadow"
auto_screening = "shadow"
```

Every feature defaults to `off`. `permission_advice = "shadow"` evaluates predictions without changing a prompt. Retaining them requires `log = true`. `"advise"` can add warnings to an already visible prompt without delaying the user's answer. `auto_screening = "shadow"` leaves Auto's deterministic baseline unchanged. `"enforce"` can turn an eligible Auto call into a prompt, and only `"enforce"` lets Auto run a line that cannot be checked command by command.

Each warning is a `⚠` line above the answers, with the predicted likelihood, such as `⚠ May delete files (88%)`. The warnings are `May delete files`, `May upload or send data`, `May read or use credentials`, `May change file permissions`, `May rewrite remote history`, and `May change project files`. ACP permission requests carry the same phrases. They are uncertain predictions, not proof that a call is safe or unsafe. A model's read-only prediction never grants permission.

`shell_effect = "advise"` can warn about possible project writes during Plan review when `thresholds.shell_writes` is explicitly configured. This is caution only. Deterministic checks still decide read-only access. Shadow labels use the deterministic classifier, not observed filesystem changes.

`content_screening = "advise"` samples web and MCP results. When both injection and agent-addressed signals cross their thresholds, Caudra adds caution and tightens upload and credential screening for the session. It keeps the content available. Sampling and predictions can miss an attack, so this is not an injection barrier.

Shell duration advice applies only to eligible local native shell calls. Measured history outranks model estimates. Enforce can fill an omitted timeout and choose delivery at admission, but never changes an explicit timeout or promotes a running synchronous call based on elapsed time. A filled timeout appears in the permission prompt, and approving the call approves that timeout. Endless predictions give caution only. See [shell duration configuration](/docs/configuration/#shell-duration) for the limits and separate history storage.

Set `log = true` to retain bounded, redacted decision states and answers in decision-log tables in `caudra.db`. A state over the bound is never sent. Its row records a `rejected` error and the state's size in place of the state. Redaction is best effort both before transmission and before storage. It is not a guarantee that sensitive text has been removed. Human permission answers can supply training labels, but approval is not proof that a predicted effect occurred. Effect fields are a partial action record. A value of `none` does not prove that no advice or routing was applied. Tool-search records do not have actual-use labels, and shell-effect records do not have observed-filesystem labels.

`/decisions` opens a read-only inspector with four tabs. Overview shows the engine, its cached health, the session's permission mode and taint, logging, and thresholds. Features lists each feature with its mode, allowed modes, and thresholds. Activity counts logged decisions per feature, and Recent lists the newest logged decisions with their state, questions, answers, and label. Press `g` to switch Activity and Recent between this session, this project, and every logged row. Both tabs read the decision log in `caudra.db`. Recording new rows requires `log = true`. The inspector reads the log when it opens and on `Ctrl+R`. It does not contact the endpoint. When the status bar shows `[decisions offline]`, click it to open the inspector.

Use [`caudra decisions`](/docs/cli/#caudra-decisions) to inspect configuration and manage the log. `purge --yes` deletes decision records and labels from the live tables without deleting sessions, messages, or shell duration history. This is not secure erasure. SQLite free pages, WAL files, and backups can retain copies. Review exports before sharing them. Only `decisions/permission.json` in the global configuration directory currently supports a question-file override. See [question overrides](/docs/configuration/#question-overrides).

## YOLO mode

`/yolo` and `--yolo` skip prompts after deny rules and hard restrictions have run. The status bar shows `[yolo]` while enabled, or `[!]` when the terminal is too narrow to spell it. The warning is the last chip its row gives up, so a narrow terminal drops the reasoning level before it. Clicking it turns YOLO off and brings prompts back, from a task footer as well as the main one.

An explicit `/yolo` choice is stored with the root conversation. A user-created fork and `/new` start without that explicit state. `--yolo` selects YOLO at startup.

YOLO approves each call once and stores no rule, so turning it off brings back the prompts your rules do not cover. In plan mode it also skips the prompts for commands plan mode cannot prove read-only, for `send_message` and `publish_message`, and for `execution_environment`. Plan mode still refuses what it always refuses, such as file writes outside the plan.

Decision-engine screening never interrupts YOLO, including in plan mode. Engine advice is not shown in YOLO. Deterministic denies and hard capability restrictions still apply.
