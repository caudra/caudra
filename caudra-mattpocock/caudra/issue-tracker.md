# The issue tracker

`to-spec` and `to-tickets` publish to the project's issue tracker, and `code-review` fetches the spec a change implements from it. Choose the tracker separately from the transport used to reach it.

## Choose the tracker

1. **The user or project says.** Use the user's explicit destination; otherwise read tracker guidance in `AGENTS.md` or `CLAUDE.md` (often `## Agent skills`) and any referenced file. Resolve conflicting destinations before writing.
2. **The remote hints.** Inspect the git remote when guidance is absent. A GitHub or GitLab remote suggests a forge, not proof that the project uses its issues; it may use Linear or another tracker instead. Keep credentials out of reported remote URLs.
3. **No tracker selected.** Propose persistent local Markdown under `.scratch/`.

Confirm an inferred tracker with the user before the first write, using `question` when available with an answer channel; otherwise present the question in plain text, with the inferred choice recommended. Deferred, skipped, dismissed, or unanswered questions remain unresolved and do not authorize dependent work or publication. Reuse the confirmed choice unless the project or request changes. Recording it in `AGENTS.md` is a separate, authorized Build-mode edit, not part of confirmation.

## Drafts and publication

Publishing, even to local files, needs Build mode and authorization for the destination and content; switching modes alone is not approval. In Plan mode, the main agent reads the session plan and merges the spec or ticket drafts with `plan`, preserving unrelated sections. A task returns its draft for the main agent to merge; it does not write the plan.

Before authorized publication, reread the latest draft from the current plan or file and the existing destination, reconcile any changes, and check for already-created issues or files; history excerpts are not current artifact truth. Redact secrets and unnecessary personal information. Verify created identifiers, bodies, and relationships before reporting publication; distinguish a saved draft, a partial publication, and a completed publication. Keep confirmed IDs on partial failure so retries do not duplicate issues. Publication does not authorize implementation, commits, or pushes.

Labels: apply one only when the project names it, as the `## Agent skills` section may for agent-ready work (`ready-for-agent`). Never create a label.

## Choose an available transport

Use the project's preferred transport if available. An enabled MCP server and an authenticated CLI are alternatives for the same tracker; neither selects the tracker. Do not assume either is installed, enabled, authenticated, or authorized.

- **MCP through `code`**: read `help("mcp")`, inspect the session's server list or `mcp.servers()`, and discover relevant tools with `mcp.search("issue tracker read create issues dependencies")`. Read `help("<discovered namespace>")` and `help("<discovered namespace>.<tool>")` for exact signatures before calling the discovered async method with `await` and Python arguments. Use only advertised operations and identifier types; do not invent a namespace, API, or relationship operation. No enabled server means no MCP transport.
- **CLI through `shell`**: check the installed CLI's help and authentication for the selected host/project. Its token or keyring variables must be forwarded with `/env` or `[agent.shell_env] pass`; never print their values. Confirm the repository explicitly rather than trusting a remote-derived default.
- **Neither usable**: explain the missing capability and offer persistent local `.scratch/` artifacts. Confirm this destination change rather than silently treating local drafts as publication to the requested tracker.

For parent/child links and blocking edges, use native relationships only when supported by the discovered tool or installed CLI and authorized. Otherwise put `Part of <parent>` and `Blocked by: <identifiers>` in the child's body. Leave the parent's content and state unchanged. Verify the relationship rather than assuming creation established it.

### GitHub CLI

- **Create**: `gh issue create --title "..." --body-file <file>`, with the body written to a file in the scratch directory first.
- **Read**: `gh issue view <number> --json number,title,body,labels,comments`.
- **Relationships**: inspect installed help for supported parent/child and dependency operations. If none is available, use the textual references above; do not guess API endpoints or confuse issue numbers with database IDs.
- A bare `#42` may be an issue or a pull request: try `gh pr view 42`, then `gh issue view 42`.

Supply the confirmed repository with `--repo <owner>/<repo>` for these issue/PR commands when the clone's default is not the selected destination.

### GitLab CLI

After checking installed help and the selected repository: `glab issue create --title "..." --description "..."`, `glab issue view <number> --comments`, and a `Blocked by: #<n>, #<n>` line for blocking edges when no supported native operation is available.

## Local Markdown

- One feature per directory: `.scratch/<feature-slug>/` in the workspace. This is persistent project work, not the environment's temporary scratch directory; file writes still need Build mode.
- The spec is `.scratch/<feature-slug>/spec.md`.
- One file per ticket at `.scratch/<feature-slug>/issues/<NN>-<slug>.md`, numbered from `01` in dependency order, never one combined file.
- A ticket's state is a `Status:` line near its top; its blockers a `Blocked by:` line.
- To fetch a ticket, read the file at the path or number the user gives.
- Read existing files before editing; preserve unrelated content. Verify the resulting files before reporting them as published locally. Do not commit or push them without authorization.
