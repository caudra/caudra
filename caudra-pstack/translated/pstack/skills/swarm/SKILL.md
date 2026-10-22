---
name: swarm
description: "Fan out N parallel workers, drain them, and return one report. Use for /pstack:swarm, 'swarm this', or parallel coverage, races, gauntlets, and exploration."
disable-model-invocation: true
---

# Swarm

## Start

Open a `todo_write` list with one entry per phase before launching anything.

1. Frame
2. Fan out
3. Aggregate
4. Report

## Phase A: Frame

1. State the done predicate and the artifact or report the swarm must return.
2. Choose the shape. Partition into slices, race N workers on identical briefs, or mix both. For a race or mixed shape, declare `first pass`, `rank all`, or `best-of` before spawning.
4. Pick the worker model from the Verify row of the **Model roles** table below. Change it in `/settings` → pstack. For `inherit`, omit `model` and `thinking` so the workers run on the model Caudra gives a task with no `model`: the profile's `subagent_model`, else the user's Subagent model. If `task` rejects a value, run the workers as `inherit` and say so. If `task` has no `model` or `thinking` argument, omit both and say so. For a model race, name each arm's model up front. Pick each subagent's reasoning level for the job, per the Subagents section of the **poteto-mode** skill.
5. Give each worker its own writable output when it writes. When workers verify or measure commits, each brief names the exact SHAs. A measurement brief also names the method (sample count, what one sample is, order). The worker records both in its result.

## Phase B: Fan out

Spawn all N workers in one message as `task` calls with `profile: "builtin"`, `background: true` (omitted when `task` has no `background` argument), and the step 4 `model` and `thinking`, left unset for `inherit`. Each brief tells its worker to `git worktree add` a new branch under the scratch directory, off the default branch, and to work only there.

When a worker must start from a non-default pushed branch, its brief names that branch as the worktree's base.

Every brief stands alone. Include the goal, scope, exact slice or race arm, how to verify, and what to report. Reports use `PASS`, `ISSUES`, or `BLOCKED` with evidence. A worker that can prove a defect reports `ISSUES` and lists every issue it can prove, not only the first.

If a worker drops out, proceed with N-1 and note it.

## Phase C: Aggregate

Read the terminal results. Drop a result that does not record the SHAs and method its brief names, and respawn that worker once. After a second miss, record a gap. A gap does not count as a pass. For coverage, every required slice needs a result. For a race, apply the selection rule declared up front. Use first pass, rank all, or best-of. Do not paste raw worker dumps.

## Phase D: Report

Return one consolidated in-chat report with the table, issue one-liners, gaps or dropouts, and the race rule when used.
