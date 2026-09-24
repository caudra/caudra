+++
title = "Remote Workspaces"
weight = 34
[extra]
group = "Guides"
+++

# Remote workspaces

Connect Caudra to a Workcell server when the repository and execution environment live on another host. Caudra keeps its terminal UI, model connections, and session state on the client. Workspace operations run at the selected endpoint.

Use compatible client and server builds. A matching version label alone does not establish compatibility. The pinned Workcell dependency includes reviewed-transfer contracts, with a hard break from older remote contracts. Incompatible persisted state fails without migration. See [compatibility and release status](/docs/sandboxes/#compatibility-and-release-status) for the exact pin and supported state formats.

This guide covers direct Workcell connections, where you provision the host and manage its process, credentials, TLS and storage. For optional e2b-libvirt lifecycle management, profiles, the template catalog and reviewed file transfers, use [Managed Sandboxes](/docs/sandboxes/). A failed remote connection never switches execution to the local checkout.

## Prepare the server

Use a matching Workcell build with a working Python execution worker. A generic MCP endpoint or a read-only Workcell server is insufficient. Caudra requires the full first-party catalog with matching schemas, contract and result versions, annotations, and presentation metadata.

Live discovery must advertise these capabilities. Capability contracts use version `v1`, and declared limits must pass Caudra's bounds checks. Listed methods and guarantees must be enabled unless stated otherwise.

| Capability | Required methods and guarantees |
|------------|---------------------------------|
| Control plane | `controlPlane = true`, empty `controlPlaneMissing`, and `executionEnvironment` disclosure |
| `operations` | `exactPreparation`, methods `prepare`, `execute`, `status`, `cancel`, `release`, and nonzero ledger and bounded progress-replay limits |
| `workspace` | `resolveDirectory`, `stat`, `list`, `readText`, `searchText` |
| `workspaceMutation` | `prepared` and `rollbackOnFailure` |
| `directExec` | `prepared` and `interactive = false` |
| `watch` | `open`, `poll`, `close`, and `recursive` |
| `projectAssets` | `discover` and `read` |
| `scm` | `discover`, `status`, `log`, `diff`, `readSide`, `stage`, `unstage`, `discard`, and `preparedMutations` |
| `snapshots` | `capture`, `inspect`, `status`, `prepareRestore`, `prepareUnrevert`, `acknowledge`, `prepareCleanup`, and `durablePerFileJournal` |
| `reviewedTransfer` | `privateStaging`, `sealedPublication`, `conditionalDownload`, `singleRange`, `durableOutcomes`, `createsDirectories`, and `safeInventory` |

Reviewed transfer also requires positive file, staging, I/O, lifetime, buffer and journal limits, with `maxJournalStorageBytes >= maxJournalBytes`. Binary reads use reviewed downloads. The old raw upload/download tools cannot substitute for these capabilities.

Missing capabilities fail startup, even if you disable the corresponding model tools. Keep execution-environment disclosure enabled. Do not pass `--no-expose-execution-environment`.

On the server, create private snapshot and transfer directories outside the exposed workspace. The paths below are examples to replace with your deployment paths. The token file must contain a bearer token of at least 32 bytes, supplied through your secret-management process.

```bash
install -d -m 700 /var/lib/workcell/snapshots /var/lib/workcell/transfers
workcell-mcp /srv/workspaces \
  --transport http --http-bind loopback --port 3001 \
  --http-token-file /etc/workcell/token \
  --tool-group files --tool-group web --tool-group shell \
  --tool-group python_execution --tool-group code_graph --tool-group transfer \
  --allow-write --shell-policy /etc/workcell/shell-policy.toml \
  --remote-server-id dev-server \
  --remote-workspace-id dev-workspace \
  --remote-workspace-generation generation-1 \
  --remote-root-project-id dev-project \
  --remote-principal-id developer \
  --snapshot-root /var/lib/workcell/snapshots \
  --transfer-root /var/lib/workcell/transfers
```

The snapshot directory must already exist, be absolute, belong to the server process identity, have no symlink components, and be inaccessible to group and other users on Unix. It must not overlap the workspace. Snapshot blobs and restore journals stay there, separate from client session records.

All five `--remote-*` identifiers and HTTP authentication are required for remote discovery. Keep the workspace generation stable across ordinary process restarts. Change it whenever you replace or reset the workspace. The server generates a separate process-instance identifier to detect lost volatile operation state.

`--http-token-file` and `WORKCELL_MCP_HTTP_TOKEN` are alternatives and cannot be combined. `--http-bind container` binds all interfaces and requires authentication. Workcell does not terminate TLS. Put an HTTPS reverse proxy in front of it for non-loopback access and configure `--allowed-host` for the authority forwarded by that proxy. The client endpoint is the `/mcp` URL. File transfer must reach the same authenticated server.

The shell policy file is operator-owned TOML. For example:

```toml
version = 1
default = "deny"
allow = ["git status*", "git diff*", "cargo test*"]
deny = ["git push*"]
```

Workcell's `--yolo` permits unmatched shell scopes while preserving explicit denies. It is separate from Caudra's `--yolo`. Use OS isolation and network policy for containment. Shell policy and file-root checks do not sandbox arbitrary programs.

## Configure a profile

Store the same bearer token on the client:

```bash
caudra auth workcell set dev
```

Use the hidden prompt, or `caudra auth workcell set dev --stdin` with a secret source piped into stdin. Do not put the token in an endpoint URL or command argument. Credentials are stored under the client's persistent state directory in owner-only files, without OS-keyring encryption. `caudra auth workcell list` lists names and timestamps. `caudra auth workcell delete dev` removes the saved credential.

Create `workcell.toml` in the local user configuration directory, alongside global `init.lua`. On Linux the default is `~/.config/caudra/workcell.toml`. See [platform configuration paths](/docs/configuration/#directory-layout).

```toml
version = 1

[workcell.profiles.dev]
endpoint = "https://workcell.example/mcp"
cwd = "projects/app"
credential_ref = "credential:dev"
expected_server_id = "dev-server"
expected_workspace_id = "dev-workspace"
```

`endpoint`, `cwd`, and `credential_ref` are required in each profile. The two `expected_*` fields are optional identity checks. There is no `[profiles.NAME]` shorthand. Unknown fields and unsupported file versions are rejected. The profile file must be a regular, non-symlink file owned by the user and not writable by group or other users on Unix. `chmod 600 ~/.config/caudra/workcell.toml` satisfies the permission requirement.

Endpoints must use HTTPS, except HTTP on numeric loopback addresses such as `127.0.0.1` or `[::1]`. `http://localhost` is rejected. User information, query strings, and fragments are forbidden. Non-loopback endpoints require a saved `credential:NAME` reference. Raw tokens, `env:` references, and file references are not accepted selectors.

## Tokens that live and die with a sandbox

A local sandbox manager that mints a fresh bearer token per sandbox has nothing worth saving. Set `CAUDRA_WORKCELL_TOKEN` in the environment of the Caudra process instead:

```bash
CAUDRA_WORKCELL_TOKEN="$token" caudra \
  --workcell-endpoint http://127.0.0.1:49983/sandboxes/"$id"/mcp \
  --workcell-cwd projects/app \
  acp
```

The token is read once at startup and never written to the credential store. A saved `--workcell-credential-ref` takes precedence, so an inherited variable cannot override an explicit selector. Because selection already refuses a non-loopback endpoint without a saved reference, this variable reaches numeric loopback endpoints only.

Prefer a saved credential for anything long-lived. This path exists for tokens whose lifetime is shorter than the machine they authenticate to.

Direct numeric-loopback selection can omit the credential reference at the CLI parser level. The Workcell remote discovery extension still requires an authenticated server, so use a credential for a working connection. Profiles always require `credential_ref`, including loopback profiles.

## Connect

```bash
caudra --workcell-profile dev
caudra --workcell-profile dev tools --names
caudra --workcell-profile dev skills --names
caudra --workcell-profile dev -p --prompt "run the focused tests"
caudra --workcell-profile dev acp
```

For a one-off connection, use `--workcell-endpoint`, `--workcell-cwd`, and `--workcell-credential-ref` together. Do not combine direct selectors with `--workcell-profile`. See the [CLI reference](/docs/cli/#remote-workcell-selection).

Remote selection applies to workspace tools and workspace UI operations. Explicitly configured client-local extensions remain local. It does not move provider authentication or model traffic to the Workcell host. Do not add the same endpoint as a generic MCP server alongside the selected Workcell backend.

## Project context and trust

Caudra fetches remote project assets through a bounded, revision-checked manifest. It does not execute remote `init.lua`, source remote environment files, or load remote MCP configuration. The client checkout's project configuration is also excluded. Global client configuration and its environment file remain local inputs.

Supported remote assets are:

| Asset        | Accepted paths and behavior                                                                                                                                                                                                                                                   |
| ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Instructions | `AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `COPILOT.md`, `.cursorrules`, `.windsurfrules`, `.clinerules`, `CONVENTIONS.md`, `GEMINI.md`, and `CODING_AGENT.md`, plus `.github/copilot-instructions.md` and `.caudra/instructions`. Directory scope determines applicability |
| Skills       | One-level `<name>/SKILL.md` under `.caudra/skills`, `.claude/skills`, `.opencode/skills`, or `.agents/skills`                                                                                                                                                                 |
| Commands     | Immediate Markdown files under `.caudra/commands`, `.claude/commands`, or `.opencode/commands`. They are prompt templates, not client shell scripts                                                                                                                           |
| Workflows    | `.caudra/workflows/*.rhai`. Scripts require client approval before execution                                                                                                                                                                                                  |
| Permissions  | The exact `.caudra/permissions.toml` file. Restrictive rules apply immediately. Allows require review                                                                                                                                                                         |

Skill and command directories use the order shown above, selecting the highest-priority remote tier. Project entries override same-named global entries. Instructions and skill text are model context, not permission grants.

Workflow approval and permission-allow trust bind to the remote authority, authenticated principal, project, resource identity, revision, and source digest. Editing the asset invalidates its prior approval. A local checkout's trust does not authorize an identically named remote file. Invalid or stale assets fail closed. Caudra approvals cannot override server policy.

An asset Caudra does not recognise and a path the host cannot read are both skipped, and the session warns you with their paths. `.caudra/permissions.toml` fails the session instead when it is unreadable or invalid. [Context](/docs/context/#in-a-sandbox) describes how workspace and client instruction files combine.

Only global client MCP configuration is loaded. Local stdio extensions require explicit trust and run with an isolated launch context, rather than inheriting the client checkout's cwd and environment. Treat global Lua plugins and approved local processes as trusted client code. Remote mode is not a sandbox for extensions.

## Client-owned documents and state

Conversation records, retained tool output, credentials, approval records, and the remote-operation journal stay on the client. Plans and memory notes also stay local, scoped to the remote workspace identity. They are not remote repository paths. `Ctrl+O` and `/memory` open them in the [workbench](/docs/workbench/#plans-memory-notes-and-prompt-drafts), which saves them back to the same local store.

Remote sessions expose `local_document_read`, `local_document_write`, and `local_document_apply_patch` for these documents. They accept opaque document references, not arbitrary local filenames. Patches require the revision from the latest read and reject stale edits. A remote `file_write` cannot be used to edit a client plan.

Remote file contents and tool output can still enter model context and retained client output. Keeping the repository remote does not mean its content stays exclusively on the server.

## Cwd and resume

The profile or CLI `cwd` is relative to the Workcell root. Use `.` for that root and POSIX-style paths for subdirectories. These selectors reject absolute paths and `..` components.

In the TUI, a standalone `cd` resolves a remote directory and refreshes its project context. It does not change the client's process directory. Remote shell commands use the selected immutable cwd handle. A directory change inside one shell invocation does not change later tool calls.

Navigation accepts `cd ..` and `cd ../sibling` relative to the current remote directory, as long as the result stays inside the exposed root. For example, from `projects/app`, `cd ../library` selects `projects/library`. Going above the root fails. A successful change persists the logical cursor for resume. SDK stream sessions accept the same standalone `cd` user messages.

Resume with the same remote selection:

```bash
caudra --workcell-profile dev --continue
caudra --workcell-profile dev --session SESSION_ID
```

Stored bindings include the endpoint origin, server ID, workspace ID, workspace generation, resource namespace, authenticated principal, and root project. They also retain cursor scope for cwd restoration and validation. A profile name or matching path alone is insufficient. Local-to-remote resume, a different authority, or a replaced workspace generation is rejected instead of silently rebinding history. Reconnect to the original identity or start a new session.

An ordinary server restart preserves durable identity but changes the process instance. Caudra must refresh volatile handles and reconcile pending operations. Reusing a generation after a workspace reset defeats this distinction, so generation management is the operator's responsibility.

## Interrupted operations

Caudra journals remote mutations before dispatch. When a reply is lost, it queries operation status rather than automatically repeating the mutation. Cancellation after dispatch is also reconciled through remote status. A timeout is not proof that nothing changed.

On connection and recovery, confirmed terminal outcomes can clear pending records. Forgotten server state, unavailable status, or an unconfirmed dispatched operation remains **indeterminate**.

The journal only records operations. A pending or indeterminate operation does not hold back later tool calls or workbench saves, in this Caudra process or in another one. Sandbox lifecycle actions, reviewed transfers, and attaching a session to a sandbox still refuse to start until the pending operations of the current workspace generation are reconciled or acknowledged.

Within one session, remote tool calls follow the local concurrency rules. Two writes to one file wait for each other and both apply, because the second is prepared only after the first has published. A write waiting at its permission prompt holds back later writes to the same file. Writes to different files run in parallel. Shell and Python calls take no file locks, so they never wait for a write and no write waits for them. Any remote call can still wait for a free operation slot when the host is at capacity.

File locks are shared within one session only, and a relative and an absolute path to one file take different locks. In those cases two writes can prepare against the same version of the file. Another session or a shell command can also change a file between preparation and publication, for example while the write waits at its permission prompt. The host then refuses to publish and leaves the file as it is. The model gets the answer a stale local edit gets: the file changed since it was last read and must be read again. Nothing was written, so the refusal leaves no pending record.

A patch that spans several files is the exception once one of its files is published. A refusal after that point leaves the patch partly applied, so it is reported as indeterminate and stays in the pending report until you acknowledge it.

An operation recorded against an earlier generation of the workspace is listed under its own heading. The host that ran it is gone, so it cannot be reconciled. Acknowledge it once you have checked its effects.

Use the recovery controller without asking the model to repeat the command:

1. Preserve the client state directory and the server snapshot store.
2. Run `/remote pending` to inspect the recorded operation IDs and states.
3. Run `/remote reconnect` after a connection loss, or `/remote reconcile` to query retained status on the current connection.
4. Inspect the remote files and process state before considering another mutation or acknowledgement.

### Recovery commands

| TUI command | Effect |
|-------------|--------|
| `/remote` or `/remote status` | Show connection state and a bounded list of pending operation IDs and states |
| `/remote pending` | Show the same status and pending-operation report |
| `/remote reconnect` | Rediscover the server, validate identity and catalog, then reconcile pending operations |
| `/remote reconcile` | Query operation status and clear confirmed outcomes without resending mutations |
| `/remote acknowledge <operation-id> --accept-possible-effects` | Clear that operation from the pending report after you have inspected it |

Acknowledgement is not cancellation, rollback, or proof of completion. The remote operation may already have changed files or may still be running. The exact `--accept-possible-effects` flag is required. Use the operation ID from the pending report, only after inspecting its possible effects. None of these commands retries a mutation.

The CLI uses the same controller without starting a model or loading project assets:

```bash
caudra --workcell-profile dev remote status
caudra --workcell-profile dev remote pending
caudra --workcell-profile dev remote reconnect
caudra --workcell-profile dev remote reconcile
caudra --workcell-profile dev remote acknowledge OPERATION_ID --accept-possible-effects
```

CLI recovery still needs a compatible, reachable server with the same identity. It is not an offline journal editor. Reconnect rejects identity or catalog changes rather than rebinding the session.

In [SDK stream mode](/docs/headless/), send a text-only user message containing the same slash command:

```json
{"type":"user","message":{"content":"/remote pending"}}
```

Caudra handles it before model dispatch and emits a `system` message with subtype `remote` and a `status` string, or subtype `remote_error` and an `error` string. To acknowledge through the SDK, send `/remote acknowledge OPERATION_ID --accept-possible-effects` as the content. These are user-message commands, not SDK `control_request` subtypes. One-shot `--print` is not this controller. Use the `remote` CLI subcommand for scripts that only need recovery.

### Snapshot recovery

Snapshot restore has a separate durable per-file journal. Remote rewind first prepares a preview and asks you to repeat rewind to confirm. Conflicts are not overwritten automatically. A partial or indeterminate restore requires recovery before more changes, and transcript updates and restore acknowledgement must also reconcile. Snapshots are not atomic across all files. See [Sessions](/docs/sessions/) for the conversation operations.
