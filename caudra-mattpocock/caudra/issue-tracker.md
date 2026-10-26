# The issue tracker

`to-spec` and `to-tickets` publish to the project's issue tracker, and `code-review` fetches the spec a change implements from it. Resolve the tracker once per session, in this order, and stop at the first that answers:

1. **The project says.** Read the `## Agent skills` section of the project's `AGENTS.md` or `CLAUDE.md`, and the file it points at, such as `docs/agents/issue-tracker.md`. Follow what it describes, including its label names.
2. **The remote says.** `git remote -v`: a GitHub remote means GitHub issues through `gh`, a GitLab remote (`gitlab.com` or a self-hosted host) GitLab issues through `glab`.
3. **Neither.** Local Markdown under `.scratch/`.

Confirm an inferred tracker with the user before the first write, as one `question` with the inferred tracker as the recommended option (in plain text when headless). Once they confirm, offer to record it in `AGENTS.md` as a one-line `## Agent skills` → `### Issue tracker` entry, so the next session skips the question.

Publishing writes outside the session plan, so it needs Build mode. In Plan mode, write the spec or the tickets into the session plan with the `plan` tool, and publish them once the user switches to Build mode. `gh` and `glab` run with the shell tool's environment, so their token or keyring variables must be forwarded with `/env` or `[agent.shell_env] pass`.

Labels: apply one only when the project names it, as the `## Agent skills` section may for agent-ready work (`ready-for-agent`). Never create a label.

## GitHub

- **Create**: `gh issue create --title "..." --body-file <file>`, with the body written to a file in the scratch directory first.
- **Read**: `gh issue view <number> --json number,title,body,labels,comments`.
- **Sub-issue of a parent**: `gh issue create --parent <parent> ...`, or `gh issue edit <parent> --add-sub-issue <child>` afterwards (`gh` 2.94+). Without sub-issues, put `Part of #<parent>` at the top of the child's body.
- **Blocking edge**: `gh api --method POST repos/<owner>/<repo>/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`, where `<blocker-db-id>` is `gh api repos/<owner>/<repo>/issues/<n> --jq .id`, not the `#number`. Where dependencies aren't available, a `Blocked by: #<n>, #<n>` line at the top of the body.
- A bare `#42` may be an issue or a pull request: try `gh pr view 42`, then `gh issue view 42`.

`gh` infers the repository from the clone's remote.

## GitLab

The same operations through `glab`: `glab issue create --title "..." --description "..."`, `glab issue view <number> --comments`, and a `Blocked by: #<n>, #<n>` line at the top of the body for each blocking edge.

## Local Markdown

- One feature per directory: `.scratch/<feature-slug>/`.
- The spec is `.scratch/<feature-slug>/spec.md`.
- One file per ticket at `.scratch/<feature-slug>/issues/<NN>-<slug>.md`, numbered from `01` in dependency order, never one combined file.
- A ticket's state is a `Status:` line near its top; its blockers a `Blocked by:` line.
- To fetch a ticket, read the file at the path or number the user gives.
