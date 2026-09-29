+++
title = "Permissions"
weight = 6
[extra]
group = "Reference"
+++

# Permissions

Caudra reviews a tool's action before it sends the call to the tool. Reusable decisions bind to a host-generated authority, the tool implementation, validated input, typed resources, and execution type.

Permissions control consent. They do not sandbox shell commands or external MCP processes.

In a [remote workspace](/docs/remote-workspaces/#project-context-and-trust), local file grants do not cover remote resources. Remote project permission denies apply immediately, while allows require review of the exact fetched asset. A remote call that can change the workspace, such as a shell command or a file write, can be allowed only as `This exact call`. Caudra approval cannot override the Workcell server's immutable policy.

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

## Permission prompts

The prompt separates the action, future scope, lifetime, context, and warnings into review cards. Context identifies why approval is needed and the requester when present. `y` allows this call once. `s` remembers the displayed scope for the conversation, and `a` remembers it for the project. Routine narrow approvals act directly, without a separate Remember screen or preliminary confirmation.

Local exact-shell calls with fully matched input and prepared context use a compact review. It shows the command, working directory, lifetime, and any warnings, with `Exact call only` and `Same reviewed preparation` describing the retained scope.

Already covered resources are collapsed behind a count. Expand them with `c` to see the covering scope and authority, such as `already allowed · project · rg *`.

Shell reviews use the supplied command text when available. Likely secret values and URL query values are masked, and terminal controls are escaped.

| Key | Action |
|---|---|
| `y` | On the main prompt, allow this exact call once |
| `s` | Allow the displayed future scope for the conversation |
| `a` | Allow the displayed future scope for the project, when available |
| `r` / `?` | Open the optional scope editor without approving |
| `v` / `F2` | Open or close Details |
| `c` | Expand or collapse already covered resources |
| `Tab` / `Shift-Tab` | Move focus between controls |
| `Enter` | Activate the focused control |
| `Up` / `Down` | With a scope control focused, select an authority or command row |
| `Left` / `Right` | With a scope control focused, narrow or widen that row |
| `p` | In the scope editor, use the scope and return to the main prompt without approving |
| `i` | In the scope editor, inspect an available argument pattern |
| `e` | In the scope editor, write a command prefix pattern |
| `A` | In the scope editor, review a global approval when available |
| `g` / `n` | Add guidance and deny once |
| `d` / `D` | In the scope editor or Details, review a project / global exact-call deny |
| `PageUp` / `PageDown` | Scroll the body when it does not fit |
| `Esc` | Return from a panel or editor. From the main prompt, deny once |
| `Ctrl-C` | Deny once |

Controls also support mouse input. Opening the scope editor or inspecting a suggestion grants nothing. On the main prompt, `y` allows this exact call once, regardless of the selected future scope.

Higher-impact reusable scopes and global decisions require an extra confirmation. It shows a frozen human-readable summary of the authority, lifetime, project binding, and any required phrases. It does not require interpreting a JSON rule. In this confirmation, `Enter` or `y` confirms the displayed decision, including reusable authority, when no phrase is required. Unrestricted URL, search, shell, and MCP authorities require a typed phrase followed by `Enter`. Incomplete or truncated authority summaries disable approval until they can be reviewed.

Advanced reviews keep distinct resource alternatives separate. All guards within an alternative apply, and unrestricted guards are labelled. Remote reviews show the trust anchor, server, workspace and generation, resource namespace, principal, remote project, scope and target keys, and authority digest. Binding IDs are percent-escaped. Display paths are labelled as display information rather than authority.

For eligible reads, the prompt starts on `This exact path` or `These exact paths`. This remembers the resource rather than the full input, so reading the same file with another `offset` or `limit` does not need a new grant. Exact-path search grants still constrain the search expression. The exact-call authority continues to require the complete original input. Protected paths, remote requests, plan restrictions, and unavailable persistence can limit the offered scopes and lifetimes.

Details presents supplied input and technical authority as scrollable, bounded named fields, with secret redaction and escaped terminal controls. Supplied edits and patches are shown as supplied, with a warning when no before-state is available. Opening Details does not run a tool or read files to construct a diff. Truncation is labelled.

A filesystem authority arrives as a ladder. Its narrowest rung covers the directories the request touched, and each step up covers the directory above, as far as the filesystem root. The rungs share one row, and `Left` and `Right` walk it, so widening changes the reach the row names rather than adding choices to scroll through. A rung reaching outside the repository is marked `outside repo`. A rung that takes in your home directory is marked `outside home` and needs the `ALLOW OUTSIDE HOME` phrase. Moving to another authority and back returns the ladder to its narrowest rung.

A URL authority is a ladder too. `Left` and `Right` walk it the same way, one path segment at a time, so a page can be scoped to the section it sits in. A request for `https://example.com/path/to/sub/page` starts at `https://example.com/path/to/sub/page/**` and widens through `https://example.com/path/to/**` and `https://example.com/path/**` to `https://example.com/**`, where the row reads `Any page on this origin`. A path deeper than eight segments offers its eight deepest prefixes and the origin. A URL with no path offers the origin alone. Reaching other origins is a separate authority that needs the `ALLOW ANY URL` phrase.

Multiple requests are queued by request ID. A subtask request cannot replace a prompt from the main agent or another subtask. Allow once resolves only the selected request. Terminals reporting key releases rearm on release. Older terminals may show `Tab, then retry` when the same key would cross into another decision.

When policy changes, each pending request rechecks its own current policy and refreshes partial coverage. Separate grants for A and B can cumulatively settle a request needing both. Automatic settlement requires complete coverage under the current deny, ask, lifetime, and workspace restrictions. Policy is checked again before execution, so stale displayed coverage cannot authorize a call. Conversation, project, and global lifetimes limit which waiting requests can share approval.

## Per-command scopes

A shell call can contain several commands. The scope editor gives each reviewed command its own row. A row starts on `this command`, which remembers that exact command in its reviewed workdir. Wider choices can include a suggested argument pattern or a token prefix such as `git status *`. Narrowing past the start reaches `this call only`, which remembers nothing for that command. Already covered rows start there and remain collapsed until expanded with `c`.

The main prompt counts unresolved rows, such as `Needs approval: 2 of 5 commands`. Every command needs authorization before the whole call is submitted. Shell effects are not transactional and are not rolled back if a later command fails.

With the scope control focused, `Left` and `Right` walk the selected row. Both ends clamp rather than wrap. Unsupported shell expressions remain subject to [exact-call review](#shell-parsing).

The scope you pick controls what Caudra remembers, never which commands are submitted. `this call only` means run now without remembering, not skip the command. Shell operators still determine execution order and conditional execution. Confirming reusable scopes stores separate rules, so `/permissions` lists commands separately and revoking one leaves the others in place. The lifetimes offered are the ones every granted row allows.

When one selected row covers another, the prompt identifies the covering row and scope without adding a duplicate rule. Narrowing the covering row restores the other row's choice. Displayed coverage from existing policy does not discard an explicitly selected grant. It is presentation information, not authorization.

Composed grants are validated together and stored atomically. A failed write leaves no partial grant set. Conversation grants are also added together after validation.

### Argument pattern inspector

When a row has a suggested pattern, press `i` in the scope editor. The inspector shows fixed command words, variable argument positions, working directory, evidence, and a current-call match preview. Patterns match parsed static arguments. They never interpolate captured values into command text.

Use `Tab` to focus controls, then arrows to select a slot or mode. The mode shortcuts are:

| Key | Mode | Coverage |
|---|---|---|
| `1` | Values | Selected observed literal values |
| `2` | Exact | One exact literal value |
| `3` | Glob | One entire argument matched by a glob |
| `4` | Regex | One entire argument matched by a regular expression |
| `5` | Any | Any one literal argument, subject to the fixed option guard |

`e` edits a constraint, `o` shows observed values, and `c` switches between observed tuples and independent combinations when available. `N` edits the pattern name and `n` edits a slot label. Names are display labels and do not change matching or rule identity. The executable, fixed arguments, argument count, and slot positions stay fixed.

For example, `cargo check -p <pattern1>` can restrict its variable argument to the observed values `caudra-agent` and `caudra-ui`. It does not cover another executable, another subcommand, or extra arguments. Option-looking values remain rejected unless the host has proved the position is data. Choosing Any does not remove that guard.

Slots with an unknown role can include arguments after fixed flags. That position does not prove the argument is data or establish the flag's arity. These values can select program operations, so the inspector and approval review caution against widening them.

Suggestions start with observed literal values and observed tuples, without wildcards. Tuples preserve combinations. If two slots were seen as `(alpha, json)` and `(beta, text)`, they allow only those pairs. Independent combinations also allow `(alpha, text)` and `(beta, json)`. Widening one slot to Glob, Regex, or Any still leaves the tuple restriction in force until you explicitly choose independence.

Glob matching is case-sensitive over UTF-8 bytes. `?` matches one byte, `*` excludes a literal `/`, and `**` can cross `/` in globset's recursive forms. Backslash escapes metacharacters, and braces and character classes use globset syntax. For example, `src/*.rs` matches `src/main.rs` but not `src/nested/mod.rs`, while `src/**/*.rs` can match both. These patterns match one complete argument without shell or filesystem expansion. The inspector displays these semantics beside the preview.

Regex uses Rust's finite-automata regular-expression engine, with whole-argument matching. Regular constructs such as alternation, groups, and repetition work. PCRE look-around and backreferences do not. Expressions, nesting, compiled size, input, and retained evidence are bounded. Invalid or oversized expressions show a compile error and cannot be used.

`p` uses a valid scope and returns to the main prompt without granting it. A known current-call mismatch must be fixed or approved once instead. Policy rechecks the complete call on approval. Glob, Regex, Any, and independent multi-slot combinations require extra review. A remembered pattern is explicit execution authority, even for an unfamiliar CLI. It never makes that command read-only or sandboxed.

### Writing your own pattern

For a token prefix instead of an argument pattern, press `e` on a command row in the scope editor. A prefix must end in `*`, hold at least one literal token before it, use at most eight tokens, and match the command on that row. Caudra refuses anything else and names the reason. This editor validates text without running it.

Two patterns are accepted with a caution:

- A pattern with one literal covers a whole program, such as `python *`. It is marked `[any invocation]` and needs the `ALLOW BROAD SHELL ACCESS` phrase. Typing a pattern Caudra offers on that row, such as `ls *`, is graded like the offered rung rather than as a broad grant.
- A pattern overlapping a builtin always-ask family, such as `git push *`, is marked `[always-ask family]`. It is allowed because those are often the commands worth shortcutting, but read it before confirming.

Grading is structural. It checks the shape of the pattern and that it matches the command, and it cannot know what a program does with its arguments. `sed -n *` grades clean, yet GNU `sed` can run shell commands through the `e` escape. Write patterns for programs whose arguments you understand.

## Suggested patterns

Caudra learns permission suggestions from tool use. Suggestions are never automatic grants or the default future scope. Live recognition needs at least three eligible command observations and can propose a pattern within one session. A compound request can supply several observations. Counts do not establish successful execution.

Automatic suggestions come only from eligible live observations or imported session history. A command without enough evidence has no suggested argument template. You can also [create a template from explicit command text](#command-templates) without history. The executable, workdir, and argument structure stay bound, and every template requires explicit approval.

Recognition compares individual commands with fixed argument structure and observed literal values. It can handle unfamiliar CLI names without a read-only executable allowlist. Payload and sensitivity guards still exclude interpreted code, unsafe expressions, and suspicious literals. It does not learn whole command sequences or generate scripts. Basic chains and pipelines are analyzed per command only where control flow and working-directory context can be established.

The local TUI loads bounded historical proposals in the background at startup, for the current tab, and after `/cd`. Loading is cached per canonical project and cancellable, and stale results are discarded. Remote and ephemeral sessions do not run history discovery. Approval and Suggested inspectors label imported history as unverified, with unknown outcomes and historical execution context. Analysis assumes standard Bash startup and uses the session's current stored cwd as an approximation. Loading these proposals does not make them verified live evidence.

Open `/permissions discover` for the Discover tab. The Rules tab contains stored rules and policy. Discover lists only proposals, which are not active permissions. Click either tab or press `Ctrl-G` to switch. Opening Discover does not force a new scan.

| Key | Discovery action |
|---|---|
| `Enter` | Inspect the selected proposal in the detail pane |
| `Ctrl-I` | Open the proposal evidence inspector |
| `Ctrl-E` | Create permission from the selected proposal |
| `Ctrl-G` | Switch between Rules and Discover |
| `Ctrl-O` | Show the discovery overview and scan diagnostics |
| `Ctrl-R` | Scan or refresh, bypassing an older cached result. An active scan is not restarted |
| `Ctrl-X` | Cancel the current scan request |

The overview reports `Not scanned`, `Loading`, `Ready`, `Partial`, `Unavailable`, or `Cancelled`. Completed scans show sample counts and limits. `Partial` lists cutoffs and exclusions, including per-parent row limits, omitted command scopes, and source obligations. `Unavailable` gives the reason discovery cannot run. Cancellation discards late results for that request and leaves active permissions unchanged. Press `Ctrl-R` to retry.

Imported proposals need at least two observations from two independent parent sessions. Repeated commands in one session are insufficient. An empty list can also reflect unsupported or sensitive commands, sample limits, or dismissed and snoozed definitions.

In Discover, select a proposal to read its command shape, constraints, evidence, and examples. `Enter` focuses its details without granting it. The separate evidence inspector wraps text and supports `Up` / `Down`, `PageUp` / `PageDown`, and mouse-wheel scrolling.

Create permission opens an editable draft. Select the currently registered local shell target and supply concrete command text and an absolute workdir for fresh host analysis. Historical tool identity and context do not authorize the new rule. Configure its input constraints and lifetime, then review and Save as described under [Stored rules](#stored-rules). A matching live prompt is not required. Opening the draft or analyzing its source grants nothing.

`Ctrl-D` dismisses the selected definition for the project, and `Ctrl-S` snoozes it for 24 hours. These preferences persist as bounded project and definition digests and leave active rules unchanged. Renaming a display label does not change the definition's identity.

Use [`caudra permissions discover`](/docs/cli/#discovering-patterns-from-history) for an explicit read-only scan and its limits. Neither background discovery nor the CLI executes historical commands.

## Plan mode

While plan mode is active, Caudra withholds the authority that would outlive the plan. Remembered project and global rules do not apply, allows from `permissions.toml` do not apply, and the prompt offers only the once and conversation lifetimes. Deny and ask rules still apply, because they only restrict access.

A conversation grant made while planning does apply for the rest of the plan. Approving broad shell authority for the conversation lets the agent keep exploring with scripts and searches instead of asking about each command. The grant stays with the conversation after you leave plan mode. Allow once covers only the current call.

## Read-only agents

A read-only agent is a subagent that may not change anything: research tasks, `task` calls in plan mode, and every agent a workflow starts in `read-only` capability mode. It sees `shell`, and Caudra judges each command line it runs rather than the tool as a whole.

A line is admitted when the classifier rules every command in it read-only and every path it touches resolves inside the project. `git diff`, `git log`, `rg`, and `wc -l` pass. Anything that writes, anything the parser could not read, and anything reaching outside the project is refused with a message naming the call and the reason, and the agent can retry with a narrower line. The same classifier answers here and in plan mode, so a command plan mode would refuse is refused here too.

The confinement check resolves symlinks before it answers, so a link checked into the repository cannot carry a read out of the project. It says nothing about what an admitted command reads inside the project: a read-only agent can still read any file you have.

## Stored rules

Use `/permissions` to manage stored conversation, project, and global rules and inspect policy. On a rule, `Enter` focuses its scope details without editing or revoking it. Details show named inputs, typed targets, context, and authority constraints. Missing values are marked unavailable or opaque. Builtin, configured, and trusted-plugin policy appears alongside stored rules. Project-config trust rows open a separate confirmation.

| Key | Manager action |
|---|---|
| `Ctrl-N` | New rule |
| `Ctrl-E` | Edit the selected stored rule, or Edit source when a verified local source is available |
| `Ctrl-U` | Duplicate the selected stored rule |
| `Ctrl-B` | Copy the selected stored rule, leaving its source unchanged |
| `Ctrl-K` | Start a separate Revoke review |
| `Ctrl-F` | Cycle All, Here, Other, and History filters. History shows revoked records |

The toolbar also supports mouse input. Wide terminals show the list and details side by side. Narrow terminals show the focused pane. `Tab` / `Shift-Tab` switches focus, and arrows or `PageUp` / `PageDown` navigate the focused pane. The mouse wheel scrolls the pane under the pointer.

Edit replaces the stored rule and retires its previous record when saved. Duplicate creates another rule without retiring the source. If a lifetime change crosses separate conversation and persistent databases, use explicit Copy instead of replacement. Copy and a later Revoke are separate reviewed writes, not an atomic move. Copy leaves every source rule active, including deny and ask rules.

### Editing saved authority

The editor has Rule, Targets, Arguments, Template, Changes, Scope, and Test sections. Use `Tab` or arrows to focus controls and `Enter` or Space to activate them. `Enter` while editing a field accepts that field only.

In Rule, choose a registered target, effect, lifetime, and project binding. Capability families are available only where offered by the target. Effects are Allow, Deny, and Ask. Saved lifetimes are Conversation, Project, and Global. Once belongs to a live prompt. Only Project lifetime takes a project binding, either the current project or an explicit absolute path. Workdir and template context are separate constraints. Changing lifetime or project binding does not rewrite them.

Authoring uses the current host catalog, not tool names or arbitrary identity text. The implemented provider supports audited local Workcell registrations, subject to the current tool filter, agent mode, and host availability. Remote, MCP, and plugin tool authority authoring is unavailable. Existing active rules remain inspectable and revocable. Label-only edits preserve their authority. Revoked records are inspection-only.

Targets offers only the selected registration's resource kinds, access types, guards, and match modes:

| Resource | Offered match modes |
|---|---|
| File or directory | Exact path, filesystem subtree, Any |
| Shell command | Exact command, token prefix, command template, Any |
| URL | Exact URL, URL origin, URL subtree, Any |
| Search query | Exact query, Any |
| Isolated Python or environment-inspection custom resource | Exact value, Any |

Shell targets can have an exact, subtree, or Any workdir guard. Directory targets offer an exact recursion guard. Glob and Regex are command-slot modes, not general resource match modes. Target alternatives are `ANY OF`, while access, protection, and attribute guards within one alternative are `ALL OF`. Removing the last target leaves an invalid blank. Unrestricted resources and wildcard guards require explicit choices.

Arguments constrains the whole tool input independently of targets. Choose exact JSON input, selected JSON pointers, or explicitly unconstrained input. Missing input is distinct from JSON `null`. Changing a target does not remove an existing input constraint. Supply replacement input or explicitly choose Keep old input pin when retaining that constraint is intended.

Stored hashes and sanitized review labels do not recover editable values. Opaque constraints remain preserved until you choose a replacement mode and enter a value. Label-only changes can keep them unchanged. New rules, copies, and authority expansions require reviewable values. A display label cannot substitute for a missing target or input.

Choose Preview (`Ctrl-P`), inspect the before/after Changes and resulting Scope, and confirm the listed authority changes when required. Save (`Ctrl-S`) is a separate action and waits for durable acknowledgment. Background validation never saves. Draft or context changes invalidate the reviewed preview and require fresh review. Saving a grant or relaxing a restriction can release pending requests after policy rechecks.

### Command templates

To create a template without a live request or historical proposal:

1. Choose New and select the registered local shell target. In Targets, add a Command target and configure its access and protection guards.
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

Use `--database` to select an existing canonical absolute `caudra.sqlite` path, and check the database path in the report before applying. Development binaries default to the separate `caudra-debug` namespace. The [CLI reference](/docs/cli/#caudra-permissions) shows how to target the production database explicitly.

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

The TUI can select broad whole-tool MCP authority for the current conversation. It requires the `ALLOW MCP TOOL` confirmation phrase and cannot be stored for a project or globally. ACP and SDK clients remain exact-only.

## Shell parsing

Bash analysis starts from the normalized initial working directory and tracks the possible directory at each command. Basic `&&`, `||`, `;`, and pipeline expressions can receive per-command scopes when control flow and context are proven. Reusable argument patterns require one known effective workdir. Unknown or ambiguous context falls back to manual exact-call review instead of assuming a directory or learning a sequence template.

The parser preserves executable directory prefixes for allow matching. `/usr/bin/git status --short` therefore does not inherit `git status *` authority. Deny and ask rules also check the normalized executable name, so `rm *` still restricts `/bin/rm`. Quotes keep argument boundaries, and a wildcard consumes complete arguments rather than arbitrary text.

A shell prompt can offer a token prefix derived from the reviewed command. A curated table names the families whose first operand is data rather than a subcommand, so `rg needle src/` offers `rg *` and keeps the search term out of the rule. The table also names the families whose subcommand sits behind a namespace token, so `npm run build` offers `npm run build *`. Outside the table the leading lowercase words become the prefix, so `git commit -m "message"` offers `git commit *`. This prefix heuristic is separate from generic argument-pattern recognition.

A derived prefix never reaches past a flag, so `docker -H tcp://host run nginx` offers no prefix. A prefix taken from outside the table must name more than the executable and must leave at least one operand behind, which is why `git status` offers no prefix. Caudra also avoids deriving prefixes that overlap a default ask family, because storing `git checkout main *` would silence the `git checkout *` ask.

Dynamic arguments, command or process substitution, unsupported control flow, wrappers such as `eval` and `sudo`, ordinary file redirects, heredocs, and parse failures require protected whole-call review. Interpreted payloads and sensitivity guards also prevent argument-pattern suggestions. Successful parsing alone does not establish reusable authority.

Configured allows, scope allows, and command patterns never cover a protected command. The two unrestricted shell authorities do, because they already authorize any command the user can write, including `tee` and an interpreter reading a script from standard input. Selecting one requires the `ALLOW BROAD SHELL ACCESS` phrase. Deny rules still apply. Builtin command-family asks do not reach protected commands, so a broad grant also silences those asks for them.

Caudra executes the reviewed command text unchanged. Workcell may reduce completed shell output before the model receives it. The TUI shows raw output while the command runs, then switches to a labelled filtered view that the user can toggle back to raw.

On Unix, Workcell launches a host-bound absolute Bash executable with `--noprofile --norc -c`. It does not implicitly load login files or shell startup hooks. The existing environment allowlist still forwards `PATH`, proxy settings, and selected basic variables. This startup contract does not isolate the commands or their own configuration. See [Shell host configuration](/docs/cli/#shell-host-configuration) for executable selection.

The initial working directory is context, not confinement. An approved shell command can still access files, the network, and inherited environment variables.

The builtin read-only classifier is conservative about mutation flags, executable paths, expansions, and named file operands. For example, `git branch -D`, tag creation, and reflog expiration do not receive read-only authority. A path-qualified executable such as `./cat` does not inherit the builtin reader allowance, and an explicitly named protected operand such as `.env` requires review. These checks do not guarantee containment of recursive reads, program configuration, or repository code. Build and test commands are not read-only exemptions.

`execution_environment` still requires explicit approval under normal prompting policy. Its fixed probes can invoke sudo policy hooks or refresh credentials. A remembered exact grant can avoid repeated prompts without treating the tool as a pure read.

## Plugin rules

Plugin rules apply only while Lua plugins run, which needs `experimental.lua_plugins`. See [Experimental features](/docs/configuration/#experimental-features).

Bundled plugins can declare trusted host policy for resources they own. Builtin allows apply only to implementations marked as bundled by the loader. User plugins do not inherit native or bundled trust. Global user plugins need a valid `plugin.toml` before they can register allow policy. Project plugins can register deny rules only. Remembered Lua decisions bind to the plugin name, tool name, entry source, required Lua modules, description, and schema. A reload during review cannot switch the approved handler generation.

Lua plugin API capabilities remain separate. `plugin.toml` controls whether plugin code may call filesystem, network, process, and environment APIs. Tool-call permissions control whether the agent may invoke a registered tool.

## Auto mode

Auto mode is experimental and belongs to the decision engine. Turn it on with `decision_engine = true` under `[experimental]` in the global `caudra.toml`, then restart Caudra. See [Experimental features](/docs/configuration/#experimental-features).

`/auto` toggles between Ask and Auto. `--auto` selects Auto at startup. The status bar shows `[auto]`, or `[a]` in a narrow terminal. Clicking the chip returns to Ask. `--auto` and `--yolo` are mutually exclusive.

Auto skips prompts only when no rule covers the call and the tool's default is Prompt. Configured asks, builtin asks, protected paths, and forced prompts still require approval. In plan mode, shell calls that deterministic checks cannot prove read-only still prompt. Deny rules and default Deny remain effective.

An explicit mode choice is saved with the root conversation. A command-line mode overrides the restored choice. Global `always_auto = true` supplies the default when neither exists. Global YOLO settings take precedence if both defaults are enabled. Projects cannot set `always_auto` or `always_yolo`.

While the switch is off, `/auto`, `--auto`, SDK `--permission-mode auto`, and an SDK `set_permission_mode` request for `auto` fail with an error, and the mode does not change. With `always_auto = true`, sessions start in Ask and Caudra shows a notice. A session saved in Auto also starts in Ask. The saved choice is kept, so turning the switch on later brings Auto back. Ask, Plan, and YOLO are unaffected.

With the switch on, Auto works without a configured decision endpoint. With enforced engine screening, a flagged call, engine error, or timeout takes the ordinary prompt path. Without a channel for answering that prompt, the call is denied. Disabling globally required screening in project configuration also restores prompting for eligible calls.

Auto is not a security boundary. An engine miss can let an eligible call run. Engine predictions never grant read-only access or weaken deterministic permission rules.

### Decision engine advice

The decision engine needs the same `experimental.decision_engine` switch, and so do `caudra decisions` and `/decisions`. While the switch is off, `[decisions]` settings have no effect, and existing decision logs and shell duration history stay untouched. Turning the switch on enables nothing by itself. The engine still needs its endpoint and a mode for each feature, and Auto still has to be selected.

Decision features are opt-in and configured in the `[decisions]` table of the global `caudra.toml`. A project may disable a feature or logging and shorten retention, but cannot redirect the endpoint, change thresholds, or enable a feature. Non-loopback endpoints require `allow_remote = true` because decision context leaves the machine. See the [configuration reference](/docs/configuration/#decisions) for fields, defaults, and supported modes.

Decision requests connect directly to the configured endpoint. They ignore ambient proxy variables and do not follow HTTP redirects, so project environment settings cannot redirect a loopback request.

HTTPS is required except for numeric loopback HTTP. A user-global `allow_http = true` permits non-loopback HTTP only together with `allow_remote = true`. Use this opt-in only when you control the transport protection, such as an encrypted tunnel. Private and CGNAT addresses do not prove that a tunnel exists. Projects cannot set either opt-in, and Workcell transport rules remain unchanged.

```toml
[decisions]
endpoint = "http://127.0.0.1:8000/v1/systemone"
timeout_ms = 400
log = false

[decisions.features]
permission_advice = "shadow"
auto_screening = "shadow"
```

Every feature defaults to `off`. `permission_advice = "shadow"` evaluates predictions without changing a prompt. Retaining them requires `log = true`. `"advise"` can add warnings to an already visible prompt without delaying the user's answer. `auto_screening = "shadow"` leaves Auto's deterministic baseline unchanged. `"enforce"` can turn an eligible Auto call into a prompt.

Warnings identify possible deletion, uploads, credential access, permission changes, remote-history rewrites, or work outside the task. They are uncertain predictions, not proof that a call is safe or unsafe. A model's read-only prediction never grants permission.

`shell_effect = "advise"` can warn about possible project writes during Plan review when `thresholds.shell_writes` is explicitly configured. This is caution only. Deterministic checks still decide read-only access. Shadow labels use the deterministic classifier, not observed filesystem changes.

`content_screening = "advise"` samples web and MCP results. When both injection and agent-addressed signals cross their thresholds, Caudra adds caution and tightens upload and credential screening for the session. It keeps the content available. Sampling and predictions can miss an attack, so this is not an injection barrier.

Shell duration advice applies only to eligible local native shell calls. Measured history outranks model estimates. Enforce can fill an omitted timeout and choose delivery at admission, but never changes an explicit timeout or promotes a running synchronous call based on elapsed time. Endless predictions give caution only. See [shell duration configuration](/docs/configuration/#shell-duration) for the limits and separate history storage.

Set `log = true` to retain bounded, redacted decision states and answers in a separate `decisions.db`. Redaction is best effort both before transmission and before storage. It is not a guarantee that sensitive text has been removed. Human permission answers can supply training labels, but approval is not proof that a predicted effect occurred. Effect fields are a partial action record. A value of `none` does not prove that no advice or routing was applied. Tool-search records do not have actual-use labels, and shell-effect records do not have observed-filesystem labels.

Use [`caudra decisions`](/docs/cli/#caudra-decisions) to inspect configuration and manage the log. Review exports before sharing them. Only `decisions/permission.json` in the global configuration directory currently supports a question-file override. See [question overrides](/docs/configuration/#question-overrides).

## YOLO mode

`/yolo` and `--yolo` skip prompts after deny rules and hard restrictions have run. The status bar shows `[yolo]` while enabled, or `[!]` when the terminal is too narrow to spell it. The warning is the last chip its row gives up, so a narrow terminal drops the reasoning level before it. Clicking it turns YOLO off and brings prompts back, from a task footer as well as the main one.

An explicit `/yolo` choice is stored with the root conversation. A user-created fork and `/new` start without that explicit state. `--yolo` selects YOLO at startup.

Decision-engine screening never interrupts YOLO, including in plan mode. Engine advice is not shown in YOLO. Deterministic denies and hard capability restrictions still apply.
