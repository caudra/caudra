# benny

benny gives you two caudra automations for slack issue reports. one triages each report. the other reproduces confirmed bugs and may prepare a small draft fix.

the files in this directory are dormant setup and automation sources. they do not appear as slash skills.

## set it up

1. point caudra at [`FOR_AGENTS.md`](./FOR_AGENTS.md) and name the target repository.
2. let setup install the `benny-triage` and `benny-reproduce` automations in the target's project automation directory. this directory is never copied into the target: its skills stay pages of the `pstack` skill. the automations need `experimental.automations`.
3. keep user-owned configuration in the target's `.caudra/benny/`. adapt [`configuration.example.yaml`](./templates/configuration.example.yaml) and [`feature-map.example.md`](./skills/reproduce-and-fix-issues/references/feature-map.example.md).
4. set `BENNY_SLACK_BOT_TOKEN` in your environment or the global `.env`, trust both automations in `/automations`, and arm them.
5. send a harmless test report and verify every source-channel post stays in the original thread.
