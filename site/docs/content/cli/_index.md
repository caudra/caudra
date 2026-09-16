+++
title = "CLI"
weight = 11
[extra]
group = "Reference"
+++

# CLI

`caudra` without a subcommand starts the TUI. Subcommands cover auth, models, MCP OAuth, updates, and a few debug helpers. Many flags only apply to one of three run paths: **TUI**, one-shot **`--print`**, or **SDK** (`--print --input-format stream-json`).

```bash
caudra [OPTIONS]
caudra <COMMAND>
```

The first message comes from `--prompt`, from piped stdin, or from both. Without `--print`, the TUI opens and that text is sent as the first message. With `--print`, Caudra runs non-interactively and exits when done.

Caudra takes no positional argument. A bare word is read as a subcommand, so `caudra mdoels` reports an unknown subcommand and suggests `models` rather than opening a session named after the typo.

When interactive Caudra starts in a Herdr pane, it automatically reports native `caudra` lifecycle state through Herdr's inherited environment and public custom-agent API. `--print` and SDK mode do not claim pane lifecycle authority.

## Flags by run path

| Flag | TUI | `--print` | SDK (`stream-json`) |
|------|-----|-----------|---------------------|
| `--prompt` | yes | yes | no (messages arrive on the wire) |
| `-m` / `--model` | yes | yes | yes |
| `--yolo` | yes | yes | yes (or `--permission-mode bypassPermissions`) |
| `--no-plugins` / `--no-commands` / `--no-jit` | yes | yes | yes |
| `--allowed-tools` / `--disallowed-tools` | yes | yes | yes |
| `--system-prompt-profile` | yes | yes | yes |
| `--ephemeral` | yes | yes | yes |
| `-c` / `--continue`, `-s` / `--session` | yes | no (always new session) | yes |
| `--exit-on-done` | yes | n/a (always exits) | n/a |
| `--image` | no (use Ctrl+V paste) | yes | via wire protocol |
| `--verbose`, `--output-format` | no | yes | stream only |
| `--system-prompt`, `--append-system-prompt` | no | no | yes |
| `--max-turns`, `--session-id`, `--fork-session` | no | no | yes |
| `--permission-mode` | no | no | yes |
| `--include-partial-messages` | no | no | yes |
| `--workcell-profile` or direct Workcell selectors | yes | yes | yes |

### Shared flags (detail)

| Flag | Description |
|------|-------------|
| `-p`, `--print` | Non-interactive run. See [Headless Mode](/docs/headless/) |
| `--prompt <TEXT>` | First message of the session. Piped stdin is appended after it when both are present. Distinct from `-p`, which selects non-interactive output |
| `--ephemeral` | Store the session, outputs, snapshots, input history, and stash in a temporary root removed at exit. Credentials, trust, and preferences remain persistent |
| `--image <PATH>` | Attach an image in `--print` mode (repeatable). Paths must be png, jpeg, gif, or webp |
| `-m`, `--model <SPEC>` | Model as `provider/model-id`. Fallback: last used → `provider.default_model` in config → auto-detect from available providers |
| `--verbose` | Full turn-by-turn messages in `--print` output |
| `-c`, `--continue` | Resume the most recent session in this directory (TUI / SDK only) |
| `-s`, `--session` / `--resume <ID>` | Resume a specific session (TUI / SDK only) |
| `--output-format <text\|json\|stream-json>` | Output shape for `--print` (default `text`) |
| `--input-format <text\|stream-json>` | With `--print`, `stream-json` enters SDK mode |
| `--no-commands` | Skip custom commands from every user and project command directory |
| `--no-plugins` | Skip user `init.lua` (global and project). The Lua host stays up and every built-in tool is native, so nothing else is lost |
| `--no-jit` | Run plugin Lua on the interpreter with full debug info |
| `--yolo` | Skip permission prompts on gated tools (alias: `--dangerously-skip-permissions`). Deny rules still apply |
| `--exit-on-done` | Exit when the agent finishes (TUI automation wrappers) |
| `--allowed-tools <LIST>` | Comma-separated allow list (PascalCase or snake_case) |
| `--disallowed-tools <LIST>` | Comma-separated deny list |
| `--system-prompt-profile <NAME>` | Select a profile from the user `system-prompts` config directory. See [System Prompt Profiles](/docs/system-prompts/) |
| `--session-id <ID>` | Session id for SDK mode |
| `--fork-session` | Load a session's history under a new id (SDK) |
| `--max-turns <N>` | Cap agent turns (SDK) |
| `--system-prompt <TEXT>` | Replace the system prompt (SDK only) |
| `--append-system-prompt <TEXT>` | Append to the built-in system prompt (SDK only) |
| `--permission-mode <MODE>` | SDK: `default`, `acceptEdits`, `plan`, or `bypassPermissions` |
| `--include-partial-messages` | Stream partial deltas in SDK mode |

### Remote Workcell selection

```bash
caudra --workcell-profile dev
caudra --workcell-profile dev --continue
caudra --workcell-profile dev -p --prompt "summarize the architecture"
caudra --workcell-profile dev acp
caudra --workcell-endpoint https://workcell.example/mcp \
  --workcell-cwd projects/app --workcell-credential-ref credential:dev
```

| Flag | Description |
|------|-------------|
| `--workcell-profile <NAME>` | Select `[workcell.profiles.NAME]` from the local user `workcell.toml` |
| `--workcell-endpoint <URL>` | Direct endpoint selection. Requires `--workcell-cwd` |
| `--workcell-cwd <PATH>` | Root-relative remote directory, such as `.` or `projects/app`. Requires `--workcell-endpoint` |
| `--workcell-credential-ref <credential:NAME>` | Saved bearer credential. Requires both direct endpoint and cwd flags |

These flags are global. A profile cannot be combined with any direct selector. With no selector, Caudra uses embedded Workcell. Selection errors and remote failures stop the operation without local fallback. See [Remote Workspaces](/docs/remote-workspaces/) for server setup, endpoint restrictions, and resume identity.

### Tool name lists

`--allowed-tools` / `--disallowed-tools` accept PascalCase (`FileRead,FileEdit,Shell`) or snake_case (`file_read,file_edit,shell`). Caudra converts PascalCase to snake_case and checks the result against the built-in tool names. Unknown names fail with the complete list of valid names.

`--disallowed-tools` also takes MCP names: `github.create_issue` for one tool, `github.*` for a server. Those keep their exact spelling. The persistent form of the same list is `agent.disabled_tools`, described in [Disabling tools](/docs/tools/#disabling-tools).

### Permission modes (SDK)

| Mode | Effect |
|------|--------|
| `default` | Normal permission prompts |
| `acceptEdits` | Accepted for Claude Code compatibility; currently same as `default` |
| `plan` | Agent mode plan with plan file `./plan.md` under cwd |
| `bypassPermissions` | Same as `--yolo` for the SDK path |

If both `--yolo` and `--permission-mode` are set, the explicit mode wins. Unknown mode names warn and fall back to `default`.

Several other Claude Code flags are accepted and ignored so existing scripts keep parsing. Caudra prints a warning when you pass one of them.

### Shell host configuration

On Unix, Workcell uses a host-bound absolute Bash executable with `--noprofile --norc -c`. It does not search `PATH` for the shell or implicitly source login files, `.bashrc`, `BASH_ENV`, or `ENV`. Commands still receive the existing environment allowlist, including `PATH`, basic home/user/locale/temp variables, and uppercase and lowercase proxy variables. Arbitrary host variables and startup hooks are not forwarded.

`WORKCELL_BASH_EXECUTABLE` selects an absolute Bash path in the execution host's environment. Runtime selection takes precedence over the same build-time variable. With neither set, Workcell tries `/bin/bash`, then `/usr/bin/bash`. An explicit empty, relative, missing, or invalid override fails instead of falling back. The selected executable is bound and revalidated before execution. A changed executable requires restarting the shell host.

```bash
WORKCELL_BASH_EXECUTABLE=/usr/bin/bash caudra
```

Set this on the Workcell host for remote execution. It is host configuration, not a model tool argument or a Caudra CLI flag. Nix builds can supply an absolute store path through the build-time variable. See [Shell parsing](/docs/permissions/#shell-parsing) for authorization limits. A predictable shell startup is not a sandbox.

## Subcommands

### `caudra permissions`

Audit permission events, discover review-only command patterns, inspect stored rules, repair review descriptions, or review an explicit permission transfer without starting an agent. `audit` and `rebind` emit JSON. `discover`, `inventory`, and `repair-review` default to human-readable output and accept `--json`.

`discover`, `inventory`, `repair-review`, and `rebind` accept `--database <ABSOLUTE_CAUDRA_SQLITE>` before or after the subcommand. The path must name an existing canonical absolute `caudra.sqlite` file. Symlink and hard-link aliases are rejected. Selecting a database creates no database or directories. Inventory, repair, and rebind reports identify the selected database path. Discovery reports a hashed source identity.

Without `--database`, commands use this build's data namespace. Development builds default to `caudra-debug`, not the production `caudra` namespace. To preview repair of the standard Linux production database with a development binary, select it explicitly:

```bash
./target/debug/caudra permissions repair-review \
  --database "$HOME/.local/state/caudra/caudra.sqlite" --json
```

Use the actual production path if your state directory differs. Keep the same `--database` selection when moving from inspection to apply.

```bash
caudra permissions audit
caudra permissions audit --log permission-sample.jsonl \
  --since 2026-09-14T00:00:00Z --max-bytes 8388608
```

`audit` reads the current canonical log unless `--log` selects another existing regular file. It rejects `--database`. It does not create directories, open the permission database, or scan rotated logs or session history.

| Flag | Effect |
|---|---|
| `--log <JSON_LOG>` | Read this file instead of the current canonical log |
| `--since <RFC3339>` | Include prompt and decision events independently at or after this timestamp |
| `--max-bytes <BYTES>` | Tail-read budget, including the boundary probe. Defaults to 8 MiB and clamps to 1 byte through 32 MiB |

The sample covers one file's bounded tail. It processes at most 50,000 complete lines forward within that tail, skipping lines larger than 64 KiB. Output reports skipped partial or oversized lines and complete bytes left unprocessed. Concurrent appends, rotation, or truncation can leave the sample incomplete.

Counts are log events, not deduplicated requests. There is no total-invocation denominator, so they cannot establish a prompt rate. Output separates explicit decisions, rule settlements, policy denials, and abandoned waits. Wait summaries pair `manager_id` and `request_id`, falling back to request ID alone when the manager ID is absent. Duplicate or colliding keys are excluded from waits. Time and sample boundaries can leave pairs incomplete. Waits include unattended time and can overlap, so they do not measure active human time. Commands, resources, identifiers, and arbitrary log strings are not emitted.

```bash
caudra permissions inventory
caudra permissions inventory --json
caudra permissions inventory --project /work/new --known-root /work/old
caudra permissions rebind --old-root /work/old --new-root /work/new
caudra permissions rebind --old-root /work/old --new-root /work/new \
  --candidates candidates.json --select FULL_RULE_ID
```

`inventory` includes persistent rules across projects and conversation rules from stored sessions, including revoked rules. Human output shows full IDs, creation and revocation times, binding status, authority, and sanitized review descriptions. Missing inputs and scopes are marked unavailable. Supplied `--known-root` paths can supply hash-verified scope labels. `--json` includes the structured constraints and digests. The project defaults to the current directory. Binding eligibility is not a full policy evaluation: it does not decide whether a particular call is allowed. Inspection does not change logical database state, though SQLite may update existing WAL coordination sidecars.

#### Discovering patterns from history

```bash
caudra permissions discover
caudra permissions discover --project /work/app --limit 10 \
  --since 2026-09-14T00:00:00Z --json
caudra permissions --database "$HOME/.local/state/caudra/caudra.sqlite" \
  discover --project /work/app --limit 5
```

`discover` reads a bounded sample of local stored tool calls and proposes command patterns for review. It never executes history, installs rules, or changes authorization. There is no apply mode. Use the [matching approval prompt](/docs/permissions/#argument-pattern-inspector) to review and grant a scope.

| Flag | Effect |
|---|---|
| `--project <ABSOLUTE_PATH>` | Match sessions whose current stored project cwd equals this path. Defaults to the current directory |
| `--limit <COUNT>` | Maximum proposals, default 10 and clamped to 1 through 64 |
| `--since <RFC3339>` | Include history records created at or after this timestamp, based on their UUIDv7 IDs |
| `--json` | Emit pattern definitions, retained literal values, evidence, per-session counts, analysis diagnostics, assumptions, exclusions, and limits |
| `--database <ABSOLUTE_CAUDRA_SQLITE>` | Read an explicitly selected database instead of the active data namespace |

The project path must be bounded absolute UTF-8 without parent components or control characters. Historical paths are interpreted lexically, without resolving them through today's filesystem. A session's current stored cwd only approximates its historical project. Timestamps describe history creation, not execution time. Invalid, missing, or future dates are excluded.

Reports label all history-derived proposals as imported, with unknown outcomes and unverified historical execution context. Analysis assumes standard Bash startup, no aliases, functions, traps, or command-not-found hook, standard builtins and directory variables, empty `CDPATH`, disabled `lastpipe`, and a logical `PWD` matching the initial cwd. `context_verified` means analysis succeeded under those declared assumptions. It does not prove the old command ran in that environment.

Proposals keep the executable, fixed arguments, argument count, and workdir bound. Variable slots start with observed literal values and their observed joint combinations, without wildcards. An unknown-role argument after a fixed flag can select a program operation. Its position does not prove it is data. See the [argument pattern inspector](/docs/permissions/#argument-pattern-inspector) before widening a slot.

The scan samples recent sessions first, with limits of 256 sessions, 10,000 history rows, 16 MiB of history, 4,096 calls, and 4 MiB of command analysis. Each parent session has a shared cap of 128 rows across main and subagent history. Reaching that cap skips the parent's remaining rows and advances to the next session. Duplicates, invalid records, and skipped rows count toward the cap. A row is limited to 256 KiB and a command to 8 KiB.

The per-parent cap prevents one large session from consuming the entire row allowance. Global byte, call, and time budgets can still end the scan earlier. Reports include visited parent-session row and byte counts, per-session cutoff flags, and the number of parents cut short. This is a bounded prefix sample, not representative coverage or a count of all omitted rows.

The two-second budget reserves half for sampling, one quarter for admitting observations, and the final quarter for suggestions and evidence. Checks run between bounded operations, so one in-flight operation can finish after its deadline. Phase cutoffs retain partial results, while cancellation discards all proposals. `--limit` changes the proposal cap, not these scan limits. Check the reported phase limits, timeout, storage availability, and exclusions before drawing conclusions.

Imported proposals require at least two observations across two independent parent sessions. Recognizer admission is first-come and capacity-bounded, so input order can change retained evidence and proposals. Capacity exclusions are reported.

Only structurally stored native shell calls and shell calls inside native batches are eligible. Tool names alone do not authenticate historical identity. Text, tool outputs, MCP calls, unsupported expressions, sensitive-looking values, and interpreted payloads are excluded. Archives and deleted history are not scanned. Repeated history IDs count once, conflicting copies are excluded, and subagents do not count as independent parent sessions. Support counts describe sampled records, not authoritative execution or success statistics.

Analysis reports aggregate omitted-command reasons and source/effect obligations, plus counts of calls with incomplete source or context. These totals precede history quarantine and recognizer admission, so observed-command counts are not retained proposal support. A matching pattern does not establish full-call authorization. Source/operator obligations and unassessed program effects can remain outside that match. Counts and matches cannot establish a safe-approval percentage.

Read-only SQLite access can update existing WAL coordination sidecars without changing logical database state. Caudra retains these sidecars after its last writer closes and truncates the WAL after a successful close checkpoint. An older offline database without sidecars is inspected only while Caudra holds exclusive storage access. Incomplete sidecars or conflicting access make the scan unavailable rather than ignoring WAL data.

Retained literals can still contain private information, so inspect JSON before sharing it. The TUI's [background discovery and Suggested list](/docs/permissions/#suggested-patterns) use imported proposals too and never grant them automatically.

#### Repairing review descriptions

```bash
caudra permissions repair-review
caudra permissions repair-review --json
caudra permissions repair-review --retry-unavailable --json
caudra permissions repair-review --apply
```

The default is a read-only dry run. This explicit, one-off repair reads old review metadata across all persistent rules and stored conversation rules. It scans bounded local session history for tool inputs and resource candidates, then verifies them against existing constraints before storing sanitized descriptions. It never executes recovered commands, resolves historical paths through the current filesystem, or changes authorization. IDs, lifetimes, effects, revocations, and project bindings remain unchanged. Already typed reviews are skipped by default.

`--retry-unavailable` also retries typed unavailable reviews and incomplete recovered reviews. Approved reviews remain untouched. Existing verified labels are retained when the new scan still cannot recover those fields. Review the retry dry run, then retain `--retry-unavailable` and the same `--database` selection when adding `--apply`.

Output reports the database path, retried, repaired, recovered, and unavailable reviews, missing scope labels and inputs, scan limits, truncation, and invalid history rows. JSON includes the path in `database`. A recovered review can still have unavailable fields. Check those counts before applying. History limits, redaction, and missing history can prevent full recovery. The command does not invert hashes or invent missing values.

Before applying, stop every Caudra session and close all storage readers using this database, including old Caudra versions and sessions in other projects. Run `caudra storage path` with the production binary to locate its database, and check the repair report's database path. Apply requires exclusive administrative access and rechecks the stored metadata before committing the replacements atomically.

When there are reviews to repair, apply automatically creates an owner-only, checked SQLite backup beside the database, named `caudra.sqlite.permission-review-<ID>.bak`. Human output prints its exact path after `SQLite backup:`. JSON output reports it in `backup`. A no-op apply creates no backup. Keep the backup private because it contains the full database, including the old metadata and session history.

After a successful apply, restart Caudra with the updated version and inspect `/permissions` or `caudra permissions inventory` against the same database. A failed access check requires closing the remaining readers and rerunning the dry run before applying again. No permission re-approval is needed for a metadata-only repair.

#### Rebinding stored rules

`rebind` defaults to a read-only preview and selects nothing automatically. Use a normalized historical absolute `--old-root` and an existing canonical destination directory for `--new-root`. The old root's current symlink is irrelevant. Repeat `--select` with full rule IDs, including affected global rules and restrictive rules you intend to transfer.

The optional candidate file is bounded to 1 MiB and uses this shape:

```json
{"paths":["/work/old","/work/old/src"],"values":["git status --short"]}
```

`paths` supplies historical absolute filesystem or workdir spellings. `values` supplies exact non-path resource text, such as a reviewed command. Candidates must match the stored hashes. They are never executed or persisted as recovered display metadata. Root paths alone cannot identify arbitrary descendant hashes. There is no automatic history recovery, hash inversion, or rewriting of paths inside command text. Exact-input and selected-input constraints require fresh grants.

Preview rows are classified as verifiable, needs candidate, unaffected, unsupported, or restrictive-policy blocker. Review the old and new authority, reasons, `partial_policy_transfer`, and `can_apply`. Unresolved or unselected affected deny/ask rules block related allows. A selected restrictive rule is copied while its original remains active at the source. Only replaced allow originals are retired. A subset transfer is not equivalent to moving the whole policy.

To apply, close every session and storage reader using this database, including sessions in other projects. Repeat the reviewed command with the same roots, candidates, and selections, adding:

```bash
--apply --confirm PREVIEW_FINGERPRINT
```

Use the top-level `confirmation` value from that selected preview, not the inventory fingerprint. Apply rechecks the preview fingerprint, destination identity, resource aliases, and database access guards. It inserts replacements and retires selected allow originals atomically, retaining source-record provenance. Changed inventory or destination identity requires a fresh preview. Rules remain path-bound after commit, not inode-bound.

This is fresh destination authorization. It never transfers project-config trust or YOLO, and it never automatically rebinds stored grants through symlinks. See [Permissions](/docs/permissions/) for runtime matching and prompt behavior.

### `caudra remote`

Inspect or recover the selected remote workspace without running a model:

```bash
caudra --workcell-profile dev remote status
caudra --workcell-profile dev remote pending
caudra --workcell-profile dev remote reconnect
caudra --workcell-profile dev remote reconcile
caudra --workcell-profile dev remote acknowledge OPERATION_ID --accept-possible-effects
```

A remote selector is required. With no action, `remote` shows status. Acknowledgement releases a local safety block and accepts that the operation may have had effects or may still be running. It does not cancel, undo, or resend the operation. See [recovery commands](/docs/remote-workspaces/#recovery-commands) for the TUI and SDK forms and the inspection steps to take first.

### `caudra auth`

```bash
caudra auth login [provider] [--method oauth|api-key]
caudra auth logout <provider>
caudra auth status
caudra auth workcell set <NAME> [--stdin]
caudra auth workcell list
caudra auth workcell delete <NAME>
```

`login` stores credentials under the state directory and can write plan or base URL choices into `providers.toml` (see [Configuration](/docs/configuration/#directory-layout) for the platform path). The picker asks for subscription OAuth or an API key when you choose Anthropic or OpenAI. Named Anthropic and OpenAI logins default to OAuth. Pass `--method api-key` to store a key instead.

Anthropic OAuth is experimental. The command explains the Anthropic terms limitation before opening the browser. OpenAI uses a device authorization flow. xAI and Copilot retain their dedicated named login flows. Other providers prompt for an API key and a plan when more than one plan exists. The interactive picker can also create custom providers.

The TUI `/login` command offers the same method choice for Anthropic and OpenAI. `status` distinguishes saved OAuth, saved API keys, environment credentials, configured endpoints, and missing credentials.

`auth workcell set` reads a bearer token from a hidden terminal prompt, or from stdin with `--stdin`. It stores or replaces a named credential, referenced as `credential:NAME`. `list` shows names and update times without bearer values. Credentials are stored in owner-only local files, without OS-keyring encryption. They are separate from provider login and MCP OAuth credentials.

### `caudra models`

```bash
caudra models
caudra models --jobs
caudra models --jobs --model anthropic/claude-sonnet-4-6
```

The plain command streams every model Caudra currently knows about from built-ins, discovery, and catalogs. Each line starts with its model spec and may end with a Small, Fast, or Best supply marker. Warnings from discovery go to stderr.

`--jobs` prints a table with **Job**, **Binding**, and **Resolved** columns for Chat, Plan, Subagent, Compact, Title, Goal, Fast, and Best. The optional global `--model` sets the anchor used to resolve the table and may appear before or after `models`. Without it, the normal saved, configured, or detected model becomes the anchor. It resolves from configuration and locally available model metadata, avoiding the all-provider discovery pass used by the plain command. See [Providers](/docs/providers/#model-jobs) for bindings, defaults, and marker meanings.

### `caudra mcp`

```bash
caudra mcp auth <server>     # OAuth for an HTTP MCP server
caudra mcp logout <server>   # drop stored tokens
```

Server names come from your [MCP config](/docs/mcp/). On a machine without a browser, `auth` prints a URL you open elsewhere and paste back.

### `caudra update` / `caudra rollback`

```bash
caudra update            # install latest release
caudra update -y         # skip confirmation
caudra update --no-color
caudra rollback          # previous version
```

Uses the same install locations as the install scripts.

### `caudra acp`

```bash
caudra acp
caudra acp -m anthropic/claude-sonnet-4-6
caudra acp --yolo
caudra --ephemeral acp
caudra --no-jit acp
```

Starts an [ACP](/docs/acp/) server on stdio for editors like Zed. Subcommand flags are only `-m` / `--model` and `--yolo`. `--ephemeral` must come before the subcommand. `-m`, `--no-jit`, `--no-plugins`, `--no-rtk`, `--allowed-tools`, and `--disallowed-tools` work on either side.

### `caudra index`

```bash
caudra index path/to/file.rs
```

Runs the native `file_index` tool and prints its compact file skeleton or directory listing. It honors `plugins.index.enabled` and `plugins.index.max_file_size_mb`. `--no-plugins` skips user `init.lua`, so default index settings apply.

### `caudra prompt`

```bash
caudra prompt                  # rendered system prompt (default: system variant)
caudra prompt research
caudra prompt general
caudra prompt --plan           # system prompt + plan-mode reminder (system only)
caudra prompt --tools          # tool definitions as JSON
caudra prompt --tools --names  # tool names only, one per line
```

Debug helper for inspecting the prompt and tool surface the agent sees. The `research` and `general` variants include the selected system prompt profile and their final host mode contract. `--plan` is rejected on non-system variants. For the tool surface, prefer `caudra tools`: it applies `--allowed-tools` and `--disallowed-tools`, and it includes MCP.

### `caudra tools`

```bash
caudra tools                                  # every tool, on, lazy, or off, with the reason
caudra tools --enabled-only                   # only the tools the model can reach
caudra tools --names                          # names, one per line
caudra tools --json                           # full records
caudra tools --schemas                        # definitions as the provider receives them
caudra tools --disallowed-tools shell         # preview a change before you run it
```

Resolves config the way a real run does, so the output reflects `agent.disabled_tools`, the plugin table, `--allowed-tools`, `--disallowed-tools`, and the model you select with `-m`. Built-in tools come first, then MCP tools grouped by server. MCP servers connect on every run, so a slow or failed server shows its status instead of its tools.

A tool that is off carries the rule that turned it off: `--disallowed-tools`, `disabled by config`, `not in --allowed-tools`, `model has no vision support`, `model uses the other editing tool`, or `no ChatGPT subscription`. A `deny` or `allow` default from [Permissions](/docs/permissions/) appears next to the tool it applies to. See [Disabling tools](/docs/tools/#disabling-tools).

A tool marked `lazy` is available and starts outside the request array, so the model reaches it through `tool_search` rather than seeing it upfront. Built-in and MCP tools can both be lazy. Which built-ins are lazy depends on the model you select with `-m`: a small or supply-unknown model defers them, while a known non-small model lists them `on` with the note `declared upfront on a known non-small model`. See [Tools loaded on demand](/docs/tools/#tools-loaded-on-demand).

### `caudra skills`

```bash
caudra skills                 # every skill with its scope and file
caudra skills git-release     # one skill's body, exactly as the model receives it
caudra skills --names         # names, one per line
caudra skills --json          # full records
caudra skills --dirs          # candidate directories: selected, superseded, or missing
```

Applies the same directory precedence a real run does, including the builtin `caudra-workflow-dev` and `caudra-plugin-dev` skills when their `plugins.skill` switches are on. `--dirs` answers why a skill is missing: a directory reads `superseded` when a higher-priority one exists, and `missing` when nothing is there. See [Skills](/docs/skills/#where-skills-live).

### `caudra logs`

```bash
caudra logs                      # the last 200 records at info and above
caudra logs -f                   # keep printing as records arrive
caudra logs -n 50 -l warn        # the last 50 warnings and errors
caudra logs --json | jq 'select(.fields.event == "retry")'
```

Prints the same structured log the `/logs` modal shows, formatted for a terminal and coloured by level when stdout is a TTY. `--json` writes the stored line back unchanged, one object per line, which is the form to pipe into `jq`. Reading the log never writes to it. See [Logging](/docs/logging/).

### `caudra storage`

```bash
caudra storage path                       # session database path
caudra storage stats [--json]             # rows, bytes, artifacts, pending cleanup
caudra storage check                      # integrity check
caudra storage sessions [--directory DIR] # list sessions with activity, size, state
caudra storage snapshots [--json] [--manifests]  # workspace snapshot stores, largest first
caudra storage trim   [POLICY | ID...] [--dry-run]
caudra storage forget [POLICY | ID...] [--dry-run] [--prune]
caudra storage prune  [--dry-run]
caudra storage pin <ID>...
caudra storage unpin <ID>...
caudra storage checkpoint [--truncate]
caudra storage vacuum [--pages N]
caudra storage usage  [--group-by GROUP] [--since DURATION] [--json]
caudra storage usage  --prune-older-than DURATION
```

`snapshots` lists the working-tree snapshot stores one row per session and workspace, largest first, so a store that has grown out of proportion to its repository is visible. A store whose workspace marker is gone is reported as orphaned rather than skipped. Snapshots stop at a nested repository the way git does, so a checkout inside your worktree is not captured with it.

`trim` demotes sessions to the transcript tier and `forget` deletes them. Both take a keep policy in `restic forget` terms and fall back to the configured `storage.retention` policy when no `--keep-*` flag is given. `prune` reclaims space that no session references. See [Sessions](/docs/sessions/#retention) for the policy rules and what each tier keeps.

Both also take session IDs instead of a policy. `caudra storage trim <ID>` is how one session's workspace snapshot store is reclaimed by hand while its conversation stays resumable, which is what `/storage` points you at when a single store has grown out of proportion. IDs and `--keep-*` rules cannot be combined, pinned sessions are still refused, and a session open in another process is skipped rather than raced.

| Flag | Description |
|------|-------------|
| `--keep-last <N>` | Keep the N most recently active sessions |
| `--keep-hourly`, `--keep-daily`, `--keep-weekly`, `--keep-monthly`, `--keep-yearly <N>` | For the last N periods that contain sessions, keep the newest session of each |
| `--keep-within <DURATION>` | Keep every session active within the duration, for example `90d` or `2y5m7d3h` |
| `--keep-within-hourly` ... `--keep-within-yearly <DURATION>` | Keep one session per period within the duration |
| `--group-by <directory\|none>` | Evaluate the policy per working directory (default) or across every session |
| `--directory <DIR>` | Only sessions for one working directory |
| `--dry-run` | Print the plan and change nothing |
| `--json` | Emit the plan and outcomes as JSON |
| `--prune` | `forget` only: run `prune` when at least one session was forgotten |
| `--unsafe-allow-remove-all` | Allow an empty policy, which keeps nothing. Requires `--directory` |

A session is kept when any rule matches. Pinned sessions, sessions open in any Caudra process, and sessions with a pending revert are never trimmed or forgotten by policy. `forget <ID>` refuses pinned sessions.

`usage` reports spend from a ledger that outlives the sessions that produced it, so trimming and forgetting leave the numbers intact. Group by `model` (default), `provider`, `project`, `purpose`, `day`, `month`, or `total`, narrow with `--since 30d`, and trim the ledger itself with `--prune-older-than`, which takes no other flag. [Token Economy](/docs/token-economy/#lifetime-spend) explains what the columns mean.

The `Hit` column is the share of prompt tokens each group read from cache, and `--json` carries it as `cache_hit_rate`. [Cache hit rate](/docs/token-economy/#cache-hit-rate) defines it.

## Everyday examples

```bash
# TUI on a project
cd ~/code/my-app && caudra

# One-shot with YOLO and a model pin
caudra -p --yolo -m anthropic/claude-sonnet-4-6 --prompt "summarize the architecture"

# Resume yesterday's session
caudra --continue

# List models, then log in
caudra models
caudra auth login

# Inspect tools without starting a session
caudra prompt --tools --names
```

For JSON / stream-json output, stdin prompts, and SDK wire mode, see [Headless Mode](/docs/headless/).
