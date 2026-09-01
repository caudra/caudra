+++
title = "Permissions"
weight = 6
[extra]
group = "Reference"
+++

# Permissions

Caudra reviews a tool's action before it sends the call to the tool. Reusable decisions bind to a host-generated authority, the tool implementation, validated input, typed resources, and execution type.

Permissions control consent. They do not sandbox shell commands or external MCP processes.

## Resolution order

Caudra resolves a tool call in this order:

1. Plan-mode and executor restrictions reject prohibited operations.
2. A matching deny blocks the call.
3. A single structured allow must cover every unresolved resource in the call.
4. Builtin and trusted-plugin policy can allow known operations.
5. YOLO mode can skip a prompt, but cannot override a deny.
6. The effective default allows, denies, or prompts.

Partial structured grants are not combined. A call that affects two resources needs one rule that covers the complete reviewed call.

## Lifetimes and authorities

Authority controls what a rule covers. Lifetime controls how long the rule remains active. The prompt selects them independently.

Exact call is the default authority. Trusted tool profiles can also offer a URL path, URL origin, filesystem subtree, shell workdir, search provider, or whole MCP tool. Caudra does not infer these choices from names in an external tool schema.

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

The prompt shows the action, risk, selected authority, typed resources, and complete validated JSON before its controls. It expands when the terminal has room and reserves space for chat. The body scrolls on smaller terminals while the controls remain visible. Likely secret values and URL query values are masked.

| Key | Action |
|---|---|
| `y` | Allow this exact call once |
| `Tab` / `Shift-Tab` | Select a host-generated reusable authority |
| `s` | Allow the selected authority for the conversation, after confirmation |
| `a` | Allow the selected authority for the project, after confirmation |
| `A` | Allow the selected authority globally, after confirmation |
| `n` | Add guidance and deny once |
| `d` | Deny this exact call for the project, after confirmation |
| `D` | Deny this exact call globally, after confirmation |
| `f` | Show technical identity and digest details |
| `Esc` or `Ctrl-C` | Deny once |

Reusable approvals are exact by default. Broad authorities require explicit selection. Unrestricted URL, search, shell, and MCP authorities also require a typed phrase. Each authority advertises its valid lifetimes. Whole-tool MCP authority is conversation-only.

Multiple requests are queued by request ID. The prompt identifies the requesting subtask. A subtask request cannot replace a prompt from the main agent or another subtask.

## Stored rules

Use `/permissions` to inspect and revoke active conversation, project, and global rules. The picker distinguishes exact, selected-input, filesystem subtree, URL subtree, URL origin, and unrestricted authority. It also shows active legacy denies and builtin, configured, or trusted-plugin policy. Read-only policy must be changed at its source.

Project and global prompt decisions are stored in `permission-rules.json` under Caudra's user state directory. The file and its update lock are owner-only. Exact input and resource values are stored as SHA-256 digests. Review metadata keeps only anonymous field positions and value types. Selected-input authorities store their JSON pointers and a digest, but never the selected values.

Conversation rules are stored with the session. A persistent write must finish before Caudra executes the approved call. If storage fails, the durable approval fails and the prompt remains open in the TUI.

## TOML policy

Caudra reads policy from:

- Global: `~/.config/caudra/permissions.toml`
- Project: `.caudra/permissions.toml`

TOML deny rules and `default = "deny"` remain active. Existing allow rules, allow defaults, and legacy conversation allows are inactive review candidates. This prevents a repository from granting itself authority and prevents an old name-only rule from authorizing a replaced tool.

A project `prompt` default cannot weaken a global `deny` default, including per-tool and MCP defaults.

`/permissions` lists these entries as `needs review`. Remove an old config entry or approve a new exact request when it appears. Legacy conversation allows can be removed directly from the picker.

```toml
default = "prompt"

[shell]
deny = [
    "sudo *",
    "rm -rf *",
]

[mcp.github]
deny = ["admin_delete"]
```

Legacy deny matching remains glob-like for compatibility:

| Pattern | Matches |
|---|---|
| `*` or `**` | Any scope |
| `prefix*` | Values starting with the prefix |
| `cmd *` | Bare `cmd` or `cmd` followed by arguments |
| `dir/**` | The directory and descendants, using path components |
| Other | Exact text |

New remembered decisions use structured matching rather than these strings.

## Typed resources

Structured requests distinguish files, directories, URLs, commands, queries, and custom resources.

- File matching uses normalized path components and resolves existing symlinks.
- Protected paths such as `.git`, `.ssh`, `.aws`, and dotenv files require exact authority.
- Access outside the project prompts even when ordinary project reads and writes are trusted.
- URL matching and fetching reject credentials and ambiguous encoded path separators or dot segments.
- Shell authority includes the initial working directory.
- A deny that intersects any resource blocks the complete call.

File-write tools remain pre-allowed inside the project working directory. Read-only filesystem tools declare scopes and trusted native policy allows them by default. Explicit deny rules can therefore block `file_read`, `file_glob`, `file_grep`, `index`, `skill`, or image access without adding normal prompt noise.

Every registered model tool reaches the permission manager. A tool without declared scopes receives its canonical validated input as an exact fallback scope. The `batch` container routes inner calls through the same manager. Native `code_execution` is isolated and has no inner tool calls.

## MCP tool calls

Generic MCP tools use the complete canonical JSON input as their exact authority. The prompt never truncates the reviewed input.

Caudra binds approval to one immutable MCP transport, server configuration digest, remote tool name, and discovered tool contract. A reconnect cannot switch the transport after approval. A changed description or schema creates a different contract.

Generic field names such as `path` or `command` do not create reusable resource authority. External servers control their schemas, so Caudra treats these values as display information unless the host has a trusted typed profile.

The TUI can select broad whole-tool MCP authority for the current conversation. It requires the `ALLOW MCP TOOL` confirmation phrase and cannot be stored for a project or globally. ACP and SDK clients remain exact-only.

## Shell parsing

Bash scopes include the normalized initial working directory. Tree-sitter walks control flow, loops, functions, and redirects so nested commands and redirect targets remain visible to deny rules.

Command substitution, process substitution, subshells, arithmetic expansion, unresolved redirect targets, and parse failures force exact review.

Caudra executes the reviewed command text. It does not replace an approved command through RTK or another post-approval rewriter.

The initial working directory is context, not confinement. An approved shell command can still access files, the network, and inherited environment variables.

## Plugin rules

Bundled plugins can declare trusted host policy for resources they own. Builtin allows apply only to implementations marked as bundled by the loader. User plugins do not inherit native or bundled trust. Global user plugins need a valid `plugin.toml` before they can register allow policy. Project plugins can register deny rules only. Remembered Lua decisions bind to the plugin name, tool name, entry source, required Lua modules, description, and schema. A reload during review cannot switch the approved handler generation.

Lua plugin API capabilities remain separate. `plugin.toml` controls whether plugin code may call filesystem, network, process, and environment APIs. Tool-call permissions control whether the agent may invoke a registered tool.

## YOLO mode

`/yolo` and `--yolo` skip prompts after deny rules and hard restrictions have run. The status bar shows `[yolo]` while enabled.

An explicit `/yolo` choice is stored with the root conversation. A user-created fork and `/new` start without that explicit state. `--yolo` supplies the initial default for a fresh root.
