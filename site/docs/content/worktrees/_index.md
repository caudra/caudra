+++
title = "Worktrees"
weight = 38
[extra]
group = "Guides"
+++

# Worktrees

A git worktree is another checkout of the same repository, with its own branch and its own directory. Caudra treats every checkout of a repository as one project. They share memory notes and plans, and a session can move from one checkout to another. `/worktree` creates, opens and removes worktrees from inside a session.

Inside a [Herdr](https://herdr.dev) pane, Herdr creates and removes the worktrees and opens each one in a workspace grouped with the repository. Everywhere else, Caudra runs git itself. The commands are the same in both cases.

## One project per repository

Caudra identifies a repository by its git common directory, the `.git` directory that all of its checkouts share. Herdr groups workspaces by the same key. A linked worktree counts only when git records it from both ends: its `.git` file names an admin directory inside the repository, and that directory names the worktree back. A crafted `.git` file therefore cannot claim another repository's state. A separate clone is a separate project, even when it tracks the same remote.

Checkouts of one repository share memory notes and plan documents. Both live under the main checkout's [project state directory](/docs/configuration/#directory-layout). Trust in project config carries over when the content is identical, as described in [Trust across checkouts](#trust-across-checkouts).

Each checkout keeps its own sessions, [scratch directory](/docs/configuration/#directory-layout) and stored permission rules. A rule can hold absolute paths, so it stays with the checkout where you granted it. To move rules deliberately, use [`caudra permissions rebind`](/docs/cli/#caudra-permissions).

A worktree may have notes and plans of its own from an older Caudra release. The first time Caudra starts in that worktree, it copies them into the shared directory. Nothing is overwritten or deleted. A name that is already taken by different content is kept beside it as `<name>.from-<branch>`.

## The worktree picker

`/worktree` lists every checkout of the repository with its branch, its location and the number of sessions that work in it. The checkout of the current session is selected.

| Key | Action |
|-----|--------|
| `Enter` | Open the selected checkout |
| `Ctrl+N` | Create a worktree |
| `Ctrl+D` | Remove the selected worktree |
| `Ctrl+R` | Refresh the list |

`/worktree new` starts creating a worktree as `Ctrl+N` does, and `/worktree new <branch>` names the branch up front. `/worktree remove` opens the removal of the checkout the session works in. The main checkout cannot be removed.

Opening a checkout outside Herdr moves every open tab into it, as [`/cd`](/docs/commands/) does. Inside Herdr, Caudra asks Herdr to open the checkout in its workspace. Herdr focuses the workspace when it is already open.

## Creating a worktree

Caudra first asks for the branch name. Leave it empty to have one generated. `/worktree new <branch>` skips this question.

Press `Enter` to open the form with **Create worktree** selected. A second `Enter` creates it. To change a field, select it and press `Enter`. Finishing an edit returns to **Create worktree**.

The form has these fields:

| Field | Meaning |
|-------|---------|
| Branch | The branch to check out. An existing branch is checked out as it is. Leave it empty to have one named for you. |
| Start at | A branch, tag or commit of the current checkout. The default is `HEAD`. |
| Carry uncommitted changes | Shown only when the current checkout has changes. |

The session that created the worktree moves into it. It works in the same subdirectory there when that subdirectory exists, and otherwise at the worktree root. The session ID and conversation stay the same, and so does its plan, because the project state is shared. Permissions approved for the conversation and YOLO mode are detached, because they were granted for the directory the session left.

Uncommitted changes carry over through `git stash`. The stash is taken in the current checkout and applied in the new worktree, so the changes leave the current checkout. Carrying works only when the worktree starts at `HEAD`, because that is the one commit where the changes are sure to apply. If applying fails anyway, the changes go back to the current checkout and the worktree is kept.

Outside Herdr, git creates the worktree under `worktrees.directory` as `<directory>/<repository>/<branch>`, with the branch turned into one path component. A branch named for you starts with `caudra/`. The process then follows the session into the worktree and reloads the project config there. See [`[worktrees]`](/docs/configuration/#worktrees) for the settings.

Inside Herdr, Herdr creates the worktree and a workspace grouped with the repository's own. Herdr names the branch when you leave it empty. The session continues in the root pane of the new workspace, and its tab closes in the current pane. A fresh session opens there when that tab was the last one.

The new pane runs the same Caudra executable. If an upgrade replaced it while Caudra was running, the pane runs the replacement at that path. If neither file remains, the pane runs `caudra` from `PATH`.

## Removing a worktree

Removal always keeps the branch. When the worktree has uncommitted changes, the confirmation offers two ways forward:

- **Stash uncommitted changes, then remove.** The changes are saved with `git stash`. Stashes are shared by every checkout of the repository, so `git stash apply` brings them back in any of them.
- **Discard uncommitted changes and remove.** The worktree is removed with `--force`.

Sessions that work in the worktree move back before it goes. They go to the same subdirectory of the main checkout when it exists, and otherwise to the main checkout root. If they cannot move, the worktree is kept and Caudra says why.

Inside Herdr, removing a worktree also closes its workspace. When the current pane is in that workspace, Caudra first opens a pane in the repository's workspace, or in a new workspace when the repository has none open. The focused session continues in that pane, and Caudra exits. The removal then runs in its own process, because Herdr closes the pane that started it.

## Moving back after a removal

A worktree can also be removed outside Caudra, by `git worktree remove`, by Herdr, or by deleting the directory and pruning. Caudra records each linked worktree while it exists, so it can still find the sessions left behind. It moves them back:

- when Caudra starts in the repository,
- when `/sessions` or `/worktree` opens,
- when `caudra --session <id>` resumes a session from a removed worktree.

A session goes to the checkout git moved the worktree to, if it was moved rather than removed. Otherwise it goes to the same subdirectory of another checkout, or to the main checkout root. A session that another Caudra process has open stays where it is until a later pass finds it free. The usage recorded for a directory moves with its sessions once all of them have moved.

## Sessions in other checkouts

`/sessions` lists the sessions of the current directory first. Below them, it lists the sessions of each other checkout of the repository, one section per checkout. Opening one of those sessions outside Herdr moves the tabs into its checkout, as `/cd` does, and then focuses the session. Inside Herdr, Caudra opens the checkout in its Herdr workspace and resumes the session in a pane there. When another Caudra process already has the session open, only the workspace is focused. See [Sessions](/docs/sessions/#moving-sessions-to-another-directory) for other ways to move sessions.

## Trust across checkouts

A checkout inherits trust that you granted at the same place in another checkout of the repository when the trusted content is identical. Inheritance works in both directions between the main checkout and its worktrees. It covers project permission config, project MCP servers and project workflows. Each kind compares its own digest, so an edit in one checkout needs fresh trust there and leaves the other checkouts trusted.

Grants stay on the exact path where you made them. Revoking trust in any checkout also revokes it at the same place in every other checkout, since each of them could otherwise still inherit it.

## What the model sees

In a linked worktree, the environment block the model receives names the branch and the main checkout, for example `- Git worktree: branch feature/login, linked to /work/app`. The line is left out when tools run on a [remote workspace](/docs/remote-workspaces/).

When a session moves to another directory, the model is told the old and the new working directory once, together with the new environment. Paths from earlier in the conversation would otherwise still look valid, and in another checkout they point at the wrong files.

Inside Herdr, the `herdr` skill is available through the [`skill` tool](/docs/skills/). Its content is what `herdr --skill` prints, so it matches the installed Herdr release. A skill named `herdr` on disk takes precedence. Shell commands get the variables Herdr sets in the pane, such as `HERDR_ENV` and `HERDR_PANE_ID`, so the skill's own Herdr check passes and its examples address this pane. Commands on a remote workspace do not get them. Suggested permission patterns for `herdr` keep three words, so approving `herdr pane read w1:p2` offers `herdr pane read *` rather than a rule for one pane.

## Herdr integration

Caudra detects Herdr from the environment Herdr gives every process it starts in a pane. Besides worktrees, Caudra reports to Herdr:

- the agent state, with the prompt that blocks it, such as `Permission requested: shell`,
- the title of the focused session, the model and the context usage, shown in the Herdr sidebar,
- the command that resumes the focused session, `caudra --session <id>`.

After a Herdr restart, Herdr runs that command in the restored pane, so the focused tab comes back. Other tabs stay available in `/sessions`. See [CLI](/docs/cli/) for how reporting works and [Notifications](/docs/notifications/#herdr) for how notifications change inside Herdr.
