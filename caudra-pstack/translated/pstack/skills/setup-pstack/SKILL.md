---
name: setup-pstack
description: Configure which models pstack uses per role and at what reasoning budget. Detects your available models and writes the role file that overrides the skill defaults. Use for /pstack:setup-pstack, "configure pstack models", "pstack budget", or changing pstack's model choices.
---

# Setup pstack

Write `~/.config/caudra/pstack/models.md`, the role file that sets pstack's model per role.

## Steps

### 1. Detect available models

The models you can pass to `task` are the session's model favorites, which the `# Model favorites` system reminder lists, and the models the user can pick in `/model`. `caudra models` lists every model Caudra knows, not only the ones this user can run, so do not detect with it. If you cannot detect any, ask the user to check `/model` and paste the models they have access to. Never write a model you have not confirmed is available. `inherit` is always valid even though it is not a detected model.

### 2. Load current state

The default role-to-model mapping is the file shape shown in step 5 below. If `~/.config/caudra/pstack/models.md` already exists, read it and treat its `# budget` line and its role values as the current choices. Otherwise start from those defaults. A line whose role is not in step 5, such as `how critics`, is from a retired role. Drop it.

### 3. Budget, map, and confirm

**(a) Ask for a budget.** Prefer the `question` tool over free text. A headless run has none, so ask in plain text then. Offer these four options with these exact labels, and name the current budget when the file records one. With no file, say that `large` matches the skill defaults.

- `unlimited — max reasoning`
- `large — xhigh reasoning`
- `medium — high reasoning`
- `small — medium reasoning`

**(b) Apply it.** Build the working table from the skill defaults, and on a re-run keep any role you changed by model, list, or `inherit`. `unlimited`, `large`, `medium`, and `small` set the thinking level of every `<model> <thinking>` value, panel entries included, to `max`, `xhigh`, `high`, or `medium`, on the ladder `max` > `xhigh` > `high` > `medium` > `low`. If the model has no such level, use its highest level at or below the target. `inherit` does not change. So `unlimited` turns `claude-opus-5-5 xhigh` into `claude-opus-5-5 max`. Grok tops out at `xhigh`, so under `unlimited` Grok stays `grok-4.7 xhigh`. `large` keeps both defaults. `small` turns them into `claude-opus-5-5 medium` and `grok-4.7 medium`.

**(c) Show the roles and confirm.** Show every role with its value, marking any model not in the detected set as needing a choice. Also list each line step 2 dropped. Ask whether to accept as-is or change specific roles, offering the detected models plus `inherit` (this role omits `model` and `thinking`, so it runs on the model Caudra gives a task with no `model`: the profile's `subagent_model`, else the user's Subagent model) as the options. Prefer the `question` tool over free text. For panel roles (arena runners, architect runners, interrogate reviewers) the value is a list, and one subagent runs per entry, `inherit` entries included, so the list length sets the count. `arena cross-judge pool` is also a list, but Arena selects one value from it whose model family differs from the parent's when possible. `swarm workers` is the default model for every worker unless a race or comparison assigns another model per arm.

### 4. Validate

Every model written must be in the detected set. `inherit` always passes. If a chosen model is not available, stop and ask again.

### 5. Write the role file

Write `~/.config/caudra/pstack/models.md` with no frontmatter: comment lines starting `#`, a `# budget: <label> (<thinking>)` line with the chosen label and its target thinking level, and one `<role>: <value>` line per role, using the same labels poteto-mode uses. A value is `<model> <thinking>`, or a comma-separated list of those for a panel. When `task` has no `model` or `thinking` argument, skills omit both and say so, so every role runs as `inherit`. Overwrite the whole file so re-runs stay idempotent. Shape:

```
# pstack model configuration. One line per role. Delete a line to fall back to the skill default.
# A value is `<model> <thinking>`: the `task` call's `model` and `thinking`. `inherit` as a value: omit `model` and `thinking`, so the role runs on the profile's `subagent_model`, else the user's Subagent model. `inherit` entries in a panel list still count toward its fan-out.
# budget: large (xhigh)
feature, refactoring: grok-4.7 xhigh
bug-fix: grok-4.7 xhigh
perf-issue: grok-4.7 xhigh
hillclimb: grok-4.7 xhigh
judgment and prose: claude-opus-5-5 xhigh
hardest tasks: claude-opus-5-5 xhigh
how explorer: grok-4.7 xhigh
how explainer: claude-opus-5-5 xhigh
why investigators: grok-4.7 xhigh
why synthesizer: claude-opus-5-5 xhigh
reflect tooling: grok-4.7 xhigh
reflect judgment, divergent, synthesizer: claude-opus-5-5 xhigh
arena runners: claude-opus-5-5 xhigh, grok-4.7 xhigh
arena cross-judge pool: claude-opus-5-5 xhigh, grok-4.7 xhigh
swarm workers: grok-4.7 xhigh
architect runners: claude-opus-5-5 xhigh, grok-4.7 xhigh
interrogate reviewers: claude-opus-5-5 xhigh, grok-4.7 xhigh
```

### 6. Confirm

Tell the user the role file was written and that its lines apply the next time the `pstack` skill is loaded, whose index shows them. Re-running this skill updates it.

### 7. Offer a verification skill (optional)

Check whether the project has a way to drive the real app for proof (a `verify-*` skill, or an existing harness). If not, offer once: "want a project-local verification skill, so agents can drive the app the way a user does and prove changes work? I can generate one with /pstack:create-verification-skill." On yes, load `pstack/create-verification-skill` with the `skill` tool and follow it. On no, move on without pushing.

### 8. Offer a formal verification skill (optional)

Check whether the project already has a formal verification skill (a project skill that runs a model checker, a proof, or another verifier against the code). If not, offer once: "want a formal verification skill, so agents can prove a key property of your code holds in every case in scope, not only the cases tests reach? I can generate one with /pstack:create-formal-verification-skill." On yes, load `pstack/create-formal-verification-skill` with the `skill` tool and follow it. On no, move on without pushing.
