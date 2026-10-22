---
name: poteto-help
description: Guides users through pstack setup, /pstack:poteto-mode, and picking the skill, playbook, or principle for a task. Type /pstack:poteto-help with a question.
disable-model-invocation: true
---

# Poteto help

Answer the user's question about pstack, hand them a prompt they can send, and link the file the answer came from. For a help question, don't start the work. The user asked how, and a pstack run spends real tokens, so let them send the prompt.

A message that asks for work, such as "use pstack to fix this bug", is not a help question. Read [`poteto-mode`](../poteto-mode/SKILL.md), do the work under it, and mention once that a system prompt profile keeps it on.

This file maps questions to the skills and guide pages that hold the answers. Those files own the details. Read the file you route to before you quote it, and trust it when it disagrees with this map. The links here point to pages of the `pstack` skill, which the user cannot open as files, so give the user the file's public copy: `https://github.com/cursor/plugins/blob/main/pstack/` followed by its path.

## Find out what they need

Infer the need from the message and the conversation. A named situation, such as "which skill reviews a PR?", goes straight to its section. If the need is still unclear, ask one multiple-choice question with these options, then answer only the section they pick:

- Get set up
- Start a task with `/pstack:poteto-mode`
- Pick a skill for a situation
- Fix a run that went wrong
- Make pstack my own

Check the state that changes the answer, and mention it only when it does:

- No `verify-*` skill or other app harness in the project means agents have no scripted way to drive the app. Mention `/pstack:create-verification-skill` when the question is about proving a change works.
- No formal verification skill in the project means no property is checked past what tests reach. Mention `/pstack:create-formal-verification-skill` when the question is about a race, an invariant, or another case tests rarely hit. When one exists and the code it covers changed after the commit it records as last in sync, mention `/pstack:maintain-formal-verification-skill` if the question touches that code.

When the question is about setup or cost, or the answer depends on which models run, say which model each kind uses from the **Model roles** table below, and that `/settings` → pstack changes them.

## Get set up

1. Turn pstack on with `pstack = true` under `[plugins.skill]` in `~/.config/caudra/caudra.toml`.
2. Optionally pick a model and thinking level for each kind of subagent in `/settings` → pstack. Unset kinds are filled from the user's favorite models and model jobs. Suggest `/pstack:create-verification-skill` and `/pstack:create-formal-verification-skill` for the project, unless it already has them.
3. Start a real task with `/pstack:poteto-mode`, a goal, and a check that can pass or fail.

Turning it on changes nothing until the user invokes a skill. A pstack skill loads when the user types `/pstack:<name>` or asks for it by name. The [README](../../README.md) and [guide page 1](../../docs/guide/01-setup.md) have the details. Offer to word their first prompt with them, per [`references/prompting.md`](references/prompting.md).

If cost is the worry, say where the tokens go and how to spend fewer. pstack spends extra tokens on subagents and review panels. In `/settings` → pstack, pick cheaper models or lower thinking levels. A kind set to `inherit` omits `model` and `thinking`, so it runs on the model Caudra gives a task with no `model`: the profile's `subagent_model`, else the user's Subagent model. That saves tokens when it is a cheaper model. A shorter Panel runs fewer subagents, one for each seat. Save `/pstack:poteto-mode` for work that needs rigor.

## Start a task with `/pstack:poteto-mode`

`/pstack:poteto-mode` matches the task to a playbook, copies the playbook's steps into the todo list with `todo_write`, and runs the other skills as the steps need them. A step it skips stays in the list as `skip: <reason>`. A good prompt states the goal and how to tell it's done. It doesn't list skills, because a hand-written sequence tends to drop or reorder steps the playbook would keep. Read [`references/prompting.md`](references/prompting.md) before you help word one. [Guide page 2](../../docs/guide/02-poteto-mode.md) has examples.

Whether `/pstack:poteto-mode` stays on depends on how the user starts it:

- `/pstack:poteto-mode` with a request loads the skill for that request. It fades as the chat moves on.
- A system prompt profile keeps it on: a file such as `~/.config/caudra/system-prompts/poteto.md` that tells the agent to load `pstack/poteto-mode` and follow it for every new task, and not for casual turns. Picked with `/system-prompt`, it stays in the prompt every turn until the user switches profile.
- Without a profile, start each new task with `/pstack:poteto-mode`.

Link [Caudra's system prompt profile docs](https://caudra.ai/docs/system-prompts/) when this comes up. Mid-chat, "new task" makes the mode match a fresh playbook. `/pstack:poteto-mode` already uses the `pstack-poteto-agent` profile for the tasks its playbook steps spawn. To get the same style from a task of your own, spawn it with `profile: "pstack-poteto-agent"`.

## Pick a skill

The default answer is `/pstack:poteto-mode`, which runs most of the others when its steps need them. Name a skill directly when the user wants more or less of something than the playbook gives. Read the skill before you recommend it, and give one example prompt.

| The user wants to | Skill |
|---|---|
| Do any non-trivial task with rigor | [`/pstack:poteto-mode`](../poteto-mode/SKILL.md) |
| Check that the agent understood their goal and the problem before it starts | [`/pstack:in-your-own-words`](../in-your-own-words/SKILL.md) |
| Get a prompt rewritten deeper, not bigger, before sending it: what a top professional would add, their raw material and methods, and an outside check. It returns the prompt and doesn't do the task | [`/pstack:sharpen`](../sharpen/SKILL.md) |
| Know how code works now, or where new code should live | [`/pstack:how`](../how/SKILL.md) |
| Know why code is shaped this way, or where a number came from | [`/pstack:why`](../why/SKILL.md) |
| Understand a change or subsystem, explained plainly | [`/pstack:teach`](../teach/SKILL.md) |
| Catch up on their own recent work on a topic | [`/pstack:recall`](../recall/SKILL.md) |
| Catch up on what changed in this chat since their last message, in plain words and small visuals, with every item waiting on them listed and decidable from the reply alone | [`/pstack:ketchup`](../ketchup/SKILL.md) |
| Know what a small diff could break outside itself | [`/pstack:blast-radius`](../blast-radius/SKILL.md) |
| Settle types and module shape before code that crosses a function boundary | [`/pstack:architect`](../architect/SKILL.md) |
| Get several attempts at one brief, merged into the best one | [`/pstack:arena`](../arena/SKILL.md) |
| Run parallel checks over slices, or race workers, as tasks whose briefs tell them to `git worktree add` a new branch under the scratch directory and work only there | [`/pstack:swarm`](../swarm/SKILL.md) |
| Have different models review a diff and try to break it | [`/pstack:interrogate`](../interrogate/SKILL.md) |
| Fix a bug test-first when a cheap local test exists | [`/pstack:tdd`](../tdd/SKILL.md) |
| Apply TypeScript rules to `.ts` or `.tsx` work | [`/pstack:typescript-best-practices`](../typescript-best-practices/SKILL.md) |
| Strip comments before review, using a reviewer that didn't write them | [`/pstack:no-comments`](../no-comments/SKILL.md) |
| Clean AI tells out of prose | [`/pstack:unslop`](../unslop/SKILL.md) |
| Write docs, an RFC, a README, a PR description, or a commit message to a standard | [`/pstack:technical-writing`](../technical-writing/SKILL.md) |
| Hear the last reply again in plain words | [`/pstack:bro`](../bro/SKILL.md) |
| Give agents a scripted way to drive the app and prove behavior | [`/pstack:create-verification-skill`](../create-verification-skill/SKILL.md) |
| Bring a verification skill and its feature map back in line with the app | [`/pstack:maintain-verification-skill`](../maintain-verification-skill/SKILL.md) |
| Formally verify a race, invariant, or other property that tests rarely reach | [`/pstack:create-formal-verification-skill`](../create-formal-verification-skill/SKILL.md) |
| Bring a formal verification back in line after the code it covers changed, and learn whether a failure is drift or a bug | [`/pstack:maintain-formal-verification-skill`](../maintain-formal-verification-skill/SKILL.md) |
| Vet a performance number before reporting or acting on it | [`/pstack:benchmark-checklist`](../benchmark-checklist/SKILL.md) |
| Run a large or cross-cutting change, or one to review after stepping away | [`/pstack:figure-it-out`](../figure-it-out/SKILL.md) |
| Keep a decision log during a run, and review it afterward | [`/pstack:show-me-your-work`](../show-me-your-work/SKILL.md) |
| Turn their own working habits into a personal mode skill | [`/pstack:automate-me`](../automate-me/SKILL.md) |
| Turn what a finished task taught into skill edits | [`/pstack:reflect`](../reflect/SKILL.md) |
| Stop agents from repeating the same mistakes in this repo | [`/pstack:correct`](../correct/SKILL.md) |
| Build a page whose buttons wake this Caudra session through an automation | [`/pstack:make-bot-ui`](../make-bot-ui/SKILL.md) |
| Find their way around pstack | `/pstack:poteto-help` |

If a skill directory next to this one is missing from the table, read its frontmatter and route by its description. The `principle-*` directories are covered under principles below.

Close calls:

- `/pstack:how` explains what the code does. `/pstack:why` explains the reasons. `/pstack:teach` runs one or both and explains the result plainly.
- `/pstack:arena` gives every worker the same brief and merges the best parts. `/pstack:swarm` splits work into slices or a race and returns one report.
- `/pstack:architect` implements right after it settles the design. Add "with checkpoint" to review the design before it writes code.
- `/pstack:interrogate` reviews the diff. `/pstack:blast-radius` looks for breakage outside the diff and proves the one fact that makes the change safe.
- `/pstack:recall` rebuilds context across recent chats. `/pstack:ketchup` covers only this chat since the user's last message. Resuming one specific chat or branch is the Session pickup playbook.
- `/pstack:figure-it-out` designs one rigorous run. The Orchestrate playbook runs a program that spans days and many PRs. The Autonomous run playbook drives one task to a finish condition.

Not in pstack:

- `/pstack:deslop`, `control-cli`, and `control-ui` come from the `cursor-team-kit` plugin, bundled beside pstack's skills.
- `/goal` is a Caudra command, and writing a skill is the `caudra-docs/skills` page.
- pstack has no `/orchestrate` skill. Orchestrate is a `/pstack:poteto-mode` playbook. If the command palette shows `/orchestrate`, a custom command or plugin provides it.

## Playbooks and principles

Playbooks are step lists inside `/pstack:poteto-mode`, not skills, so they have no slash command. Inside `/pstack:poteto-mode`, describing the task picks one, and these phrases name one directly:

- "babysit this pr" or "check on pr 123" runs Babysit. It drives the PR to merge-ready and stops there. It doesn't merge unless the user asks to merge, land, or ship.
- "land the stack" runs Shipping.
- "take over this branch" runs Session pickup.
- "pause safely" runs Pause safely.
- "full autopilot on this queue" runs Autopilot-full. "stack them, don't ship" runs Autopilot-stack.
- "run the eval playbook" runs Eval.

Without `/pstack:poteto-mode`, a phrase such as "babysit this pr" runs no playbook. The Playbooks section of [`poteto-mode`](../poteto-mode/SKILL.md) lists every playbook and when it applies. [Guide page 6](../../docs/guide/06-verify-and-ship.md) covers opening, babysitting, and landing a PR.

pstack has no planning skill. Caudra's plan mode works alongside it. For work that spans phases or stacked PRs, asking `/pstack:poteto-mode` for a plan runs the [Multi-phase plan playbook](../poteto-mode/playbooks/multi-phase-plan.md), which writes the plan and doesn't implement it. For a design question, the Prototype playbook or `/pstack:architect` settles it in code first.

Principles are one-rule skills that `/pstack:poteto-mode` reads and cites in its replies. The user rarely invokes one. They steer with the names instead, as in "apply prove it works. show me the real output." Asking for `pstack/principle-<name>` by name still loads one on demand. [Guide page 8](../../docs/guide/08-principles.md) lists them.

## Fix a run that went wrong

| Symptom | Fix |
|---|---|
| The mode stopped applying after a few turns | It was started with `/pstack:poteto-mode` on one message. Keep it on with a system prompt profile, or start each task with `/pstack:poteto-mode`. |
| A question got treated as the next step of the last task | Say "new task", or say the turn doesn't need the mode. |
| A new model choice had no effect | `/settings` → pstack applies to the next pstack page an agent loads. Load the skill again. |
| Runs cost more than expected | See the cost paragraph under Get set up. |
| A skill didn't load on its own | A pstack skill loads only when the user types `/pstack:<name>` or asks for it by name, or when `/pstack:poteto-mode` runs it, and it doesn't run every skill. |
| Parallel agents overwrote each other | Give each agent its own git worktree. |
| An overnight run moved but finished nothing | `/goal` needs a check that can pass or fail, not a duration. See [guide page 7](../../docs/guide/07-overnight.md). |
| The reply claims success from a green build | Ask for the real command, flow, stored value, or profile. That's the prove-it-works principle. |

For a run that drifts, [`references/prompting.md`](references/prompting.md) has one-line steers. [Guide page 10](../../docs/guide/10-recipes-and-pitfalls.md) has more pitfalls and the recipes worth copying.

## Make pstack my own

- [`/pstack:automate-me`](../automate-me/SKILL.md) drafts a personal mode skill from the user's own history, to use alongside `/pstack:poteto-mode`.
- [`/pstack:reflect`](../reflect/SKILL.md) after a session turns its lessons into skill edits the user approves.
- `/settings` → pstack picks the model and thinking level for each kind of subagent.
- `/pstack:poteto-mode write a skill for <workflow>` runs the authoring playbook. The eval playbook tests a skill change blind.
- Fix a misbehaving skill in its own PR, not inside the feature work where it went wrong.

[Guide page 9](../../docs/guide/09-make-it-yours.md) covers each of these.

## Reply

Lead with the answer. Give at most one example prompt in a code block, adapted from [`references/recipes.md`](references/recipes.md) when one fits, then the link to that file. Keep it short unless the user asked for the whole map.
