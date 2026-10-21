# benny automation intent

## what i want to automate

i want two caudra automations that work together in one slack issue channel.

### automation 1: triage issue reports

- trigger: when someone posts a new top-level report in my configured source slack channel, i want this automation to start on that report and keep its original thread coordinates.
- behavior: i want it to read the thread and attachments, classify the report as a bug or performance issue, feature request, question or feedback, or reroute, and trace the likely owning layer before routing.
- tracker: i want it to search my configured tracker for duplicates, update a confident duplicate, and create a ticket only for a clear net-new bug.
- tools: i want slack thread read and reply access, my configured tracker integration, and my optional routing map.
- outcome: i want exactly one reply in the source thread with a short verdict and `[benny:bug]`, `[benny:performance]`, or `[benny:other]`. a bug or performance marker may include the tracker url.
- boundary: i never want this automation to post a root message in the source channel.

### automation 2: reproduce and fix confirmed bugs

- trigger: i want this automation to watch the same channel's recent report threads and start once the trusted triage marker appears in the original thread.
- gates: i want it to stop when someone clearly owns the fix. if an existing pull request or merged commit may fix the report, i want verification instead of a competing change.
- behavior: i want it to use my configured control adapter and feature map, reproduce the exact symptom twice through the real ui, and capture screenshots, video, and a read-only state cross-check.
- fix: i want it to verify existing pull requests without authoring over them. after a confirmed repro, it may attempt one bounded root-cause fix, use tdd when the test is cheap, smoke the blast radius, and open a draft pull request only when before-and-after proof passes.
- tools: i want slack thread read and reply access, repository and history access, draft pull request creation, my configured tracker, and my control adapter.
- outcome: i want evidence and a verified result in the source or optional operations threads, plus an optional draft pull request. updates should be concise.
- boundary: i never want this automation to post a root message in the source channel.

### shared rules

- i want the source channel and root thread coordinates to stay immutable for the whole run.
- i treat utility and debug bots as evidence, not delegation or fix ownership.
- i allow subagents to help, but they cannot post to slack or receive slack credentials.
- i want this entire pack read from its pages under `pstack/automations/benny/`, never copied into the target repository. its `SKILL.md` files are direct automation instructions, not skills with slash commands.
- i want shared dependencies such as `how`, `why`, `tdd`, `unslop`, and the required principle skills loaded from the built-in `pstack` skill.
- i want each automation to tell its session to load its operational file by page address. i do not want copied excerpts or slash-skill discovery.
- i keep user-owned configuration, feature maps, and routing maps in the target repository's `.caudra/benny/`, and secrets in my environment or the global `.env`, so pstack updates cannot overwrite them.
- i want both automations to fail closed when channel coordinates, tracker access, the control adapter, or the feature map are missing or uncertain.
- i want draft pull requests only. do not merge or deploy.

### my configuration

- source slack channel: `<channel>`
- optional operations channel: `<channel or none>`
- repository and default branch: `<repo>`, `<branch>`
- tracker: `<type, team, project, labels, intake status>`
- routing map: `<path under .caudra/benny/ or none>`
- triage identity: `<slack identity>`
- control skill: `<configured skill or adapter>`
- feature map: `<path under .caudra/benny/>`
- models: `<triage, reproduce, code, media review>`
- status emoji strings: `<seen, reproducing, reproduced, blocked, fixing, failed, pull request opened>`
- budgets: `<polling, verdict wait, follow-up, repro, rejection, fix>`
- slack bot token: `BENNY_SLACK_BOT_TOKEN`, with `channels:history` on the source channel

start from [`configuration.example.yaml`](./templates/configuration.example.yaml) and [`feature-map.example.md`](./skills/reproduce-and-fix-issues/references/feature-map.example.md). copy and fill them under `.caudra/benny/` in the target repository. keep secret values in your environment or the global `.env`.

## for the agent

the human enters setup by pointing caudra at this page, `pstack/automations/benny/FOR_AGENTS.md`. do not look for or invoke a benny slash skill.

1. ask which repository will run the automations. setup and the sessions that arm the automations work in that repository.
2. treat the pages under `pstack/automations/benny/` as the source pack. never copy them into the target repository.
3. never overwrite user-owned configuration, feature maps, or routing maps in the target's `.caudra/benny/`. if ownership of an existing file is ambiguous, stop and ask before replacing it.
4. load `pstack/automations/benny/skills/setup-benny/SKILL.md` with the `skill` tool and follow it.

do not install or update an automation until i explicitly ask. both automations need `experimental.automations` in my global `caudra.toml`.

to install them, run `await automation.list()` in `code` and take the project automation directory from its `project_dir`. write the page `pstack/caudra/automations/benny-triage.rhai` to `benny-triage.rhai` there and the page `pstack/caudra/automations/benny-reproduce.rhai` to `benny-reproduce.rhai`, each with everything after the page's first header line. validate them with `await automation.validate("benny-triage")` and `await automation.validate("benny-reproduce")`. then tell me to trust both in `/automations`, to set `BENNY_SLACK_BOT_TOKEN` in my environment or the global `.env` with `channels:history` on the source channel, and to arm them, filled from the configuration:

```text
/automations arm benny-triage {"channel_id": "C…", "triage_user_id": "U…"}
/automations arm benny-reproduce {"channel_id": "C…", "triage_user_id": "U…"}
```

each automation's message tells its session to load its operational file: `benny-triage` loads `pstack/automations/benny/skills/triage-issue-reports/SKILL.md` and `benny-reproduce` loads `pstack/automations/benny/skills/reproduce-and-fix-issues/SKILL.md`, with the configuration at the `config_path` arg, `.caudra/benny/configuration.yaml` unless i arm it with another path.

for existing automations, validate the configuration, then compare each installed script with its page. if one differs, show me the diff and replace it only when i agree, then i trust it again. i change args by arming again or in the `/automations` inspector. do not create duplicates.
