# Skill mechanics

The skill-specific branch of [`writing-for-agents`](SKILL.md): what changes when the document is a skill or a command (frontmatter, the invocation choice, and router commands). Everything else about writing it is the universal reference in `SKILL.md`. For where Caudra reads each and its exact frontmatter, load `caudra-docs/skills` with the `skill` tool.

## Invocation

Two choices, trading the two loads:

- A **model-invoked** document is a skill (`.caudra/skills/<name>/SKILL.md`). Its `description` sits in the skill catalog, so the agent can fire it autonomously, and other skills can reach it. The user can still ask for it by name: model-invocation always _includes_ user reach; a description only ever adds agent discovery, never removes the human's. The description is the skill's top-level context pointer, forced to stay loaded at all times: permanent context load in exchange for discoverability. A model-invoked skill whose content is all reference is also one home for shared reference: another skill can load it, so reference needed by several skills lives in one place. Mechanics: write a model-facing description carrying the trigger branches (the pointer-writing rules in `SKILL.md` apply in full).
- A **user-invoked** document is a command (`.caudra/commands/<name>.md`), typed as `/<name>`, with `$ARGUMENTS` standing for what the user types after it. Nothing of it reaches the agent's context until the human runs it, and no skill can fire it. Zero context load, but it spends cognitive load: you are the index that must remember it exists. Mechanics: the `description` is human-facing, shown in the command palette: a one-line summary, trigger lists stripped. Caudra lists every skill in the catalog, so a skill is never user-invoked.

Pick a skill only when the agent must reach it on its own, or another skill must. If it only ever fires by hand, make it a command and pay no context load.

Shared reference that two commands both need can live in neither: a command cannot run another. Push it to a plain file outside both: external reference any skill or command can point at.

## Splitting by invocation

The invocation cut of splitting (the sequence cut lives in `SKILL.md`): split off a skill when you have a distinct leading word that should trigger it on its own (a trigger word you actually use in your prompts), or another skill must reach it. You pay context load for the new always-loaded description, so that independent reach has to be worth it.

## Router commands

When commands multiply past what you can remember, that piled-up cognitive load is cured by a **router command**: one command that names the others and when to reach for each, so the human has one command to remember instead of many. It can only hint, never fire them: nothing but the human can run a command.
