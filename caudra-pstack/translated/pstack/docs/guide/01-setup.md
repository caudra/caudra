# Set up pstack

In this page you turn on pstack, pick which models it uses, and run your first task. Setup is one config line.

## Turn on pstack

pstack is built into Caudra, so there is nothing to install. Turn it on in `~/.config/caudra/caudra.toml`:

```toml
[plugins.skill]
pstack = true
```

Caudra picks it up as soon as you save the file. Each pstack skill you run yourself is a command with a `pstack:` prefix, such as `/pstack:poteto-mode`.

## Pick your models

pstack sorts its subagents into six kinds:

- **Code**: code-writing delegates, explorers, and investigators.
- **Hardest**: the hardest changes.
- **Judgment**: prose, judgment, explainers, and synthesizers.
- **Verify**: verification lanes, including every `/pstack:swarm` worker unless a race names a model for each arm.
- **Panel**: a diverse panel for `/pstack:arena`, `/pstack:architect`, and `/pstack:interrogate`, one task per seat, so the seat count sets the panel size.
- **Cross-judge pool**: the judge that scores an arena, picked on a provider other than the parent's when possible.

Pick a model and thinking level for each kind in `/settings` → pstack. You only set what you care about. A kind you leave alone is filled from your favorite models and model jobs, using only models you can run. Every pstack page that spawns a subagent ends with a Model roles table built from these settings when the page loads, so a change applies to the next page an agent reads.

You might be wondering how to let a kind follow your own model setup. Set it to `inherit` and pstack omits the task's `model` and `thinking`, so the task runs on the model Caudra gives a task with no `model`: the profile's `subagent_model`, else your Subagent model. `inherit` isn't a model name.

## Add a verification skill

pstack proves app behavior through a `verify-*` skill or an existing harness. If your project has neither, generate one with [`/pstack:create-verification-skill`](../../skills/create-verification-skill/SKILL.md). It writes `.caudra/skills/verify-<app>/`, a project-local skill that teaches agents to drive your app the way a user does, and proves the skill works once before handing it over. [Verify and ship](./06-verify-and-ship.md#create-a-project-verification-skill) covers it in depth.

If you're new to pstack, make one. An agent that can check its own work keeps going until the check passes. An agent that can't hands every result back to you to check by hand. Of everything in this guide, the verification skill pays off the most.

Consider a formal verification skill too, with [`/pstack:create-formal-verification-skill`](../../skills/create-formal-verification-skill/SKILL.md), unless the project already has one. Tests check the cases someone thought to write. A formal verification skill checks one key property of your code, such as "a job never runs twice at once", in every case within a stated scope. It pays off when your code has concurrency, retries, a state machine, or a parser, where a bug is costly and tests rarely reach the case that breaks. Otherwise skip it, and run it later if that changes. [Verify and ship](./06-verify-and-ship.md#prove-a-property-with-a-formal-verification-skill) covers it.

## Keep the cost in check

pstack spends extra tokens on subagents and review panels. That's the price of the rigor. To spend fewer:

- In `/settings` → pstack, pick cheaper models or lower thinking levels. A strong model in the main chat with cheaper, faster models in the Code kind is a good split.
- Set a kind to `inherit` so it runs on the profile's `subagent_model`, else your Subagent model.
- Shorten the Panel. Each seat runs one subagent.
- Save `/pstack:poteto-mode` for work that needs rigor. A small, obvious edit doesn't.

## Run your first task

Pick something real but small, and describe it the way you'd describe it to a colleague:

```text
/pstack:poteto-mode add a --json flag to this command. text output stays byte-identical. verify both.
```

Watch the todo list. Its first items are the matched playbook's steps copied in, the Feature playbook for this prompt. If `/pstack:poteto-mode` skips a step, the step stays in the list with `skip: <reason>`, so you can see what it chose not to do.

From here you can type normal follow-ups. The command attaches the skill to the message you type it with, and it fades as the chat moves on.

Next: [Route work through `/pstack:poteto-mode`](./02-poteto-mode.md).
