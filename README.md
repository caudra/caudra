<p align="center">
  <img src="https://caudra.ai/social-card.png" alt="Caudra. Open source, for your terminal. A coding agent that turns smart context into effective action." width="100%">
</p>

# Caudra

**A coding agent that turns smart context into effective action.**

Caudra is a terminal coding agent written in Rust. It brings steerable subagents, built-in token savings, a full workbench, and a session history you can always return to. Everything ships in one native binary, so there are no plugins to assemble.

Leave a session working toward a goal while you are away. When you are back at the keyboard, guide any subagent, ask a side question, or open the code beside the conversation.

```sh
curl -fsSL https://caudra.ai/install.sh | sh
```

[Website](https://caudra.ai) · [Documentation](https://caudra.ai/docs/) · [Quick start](https://caudra.ai/docs/quick-start/) · [Example config](https://github.com/caudra/config)

## Why Caudra exists

> I spend most of my working day with coding agents, and I have tried many of them. Each had ideas I liked. I wanted those ideas together, along with a few new techniques, in one standalone binary I could bring into any environment.
>
> Caudra is that binary, and I push more than 20 billion tokens a month through it. With another agent, my local history grew by more than 1 GB a day. Caudra keeps over a month of my complete history in less than 2 GB, and a 10k-turn session still loads in under a second.
>
> **Thorsten Born**, software engineer and data architect, maintainer of Caudra

In the same daily use, about 97% of Anthropic and 94% of OpenAI prompt tokens came from cache. These figures are the maintainer's own measurements, not benchmarks. Timings were measured by hand, and results depend on models, projects, and hardware.

## Why the name

Caudra, pronounced KAW-druh, is named after the caudate nucleus. This part of the brain belongs to the circuits that connect evidence and goals to action. Studies link it to learning which actions lead to which outcomes ([Grahn, Parkinson and Owen, 2008](https://www.sciencedirect.com/science/article/abs/pii/S0301008208001019), [Lau and Glimcher, 2007](https://www.jneurosci.org/content/27/52/14502), [Doi et al., 2020](https://elifesciences.org/articles/56694)).

The name describes how Caudra works. It turns the context of your task into an action, then reads the outcome before it chooses the next one. The brain is only the inspiration for the name. Caudra is software, and it works toward the goal you set.

## A quick tour

### Guide any agent while it works

The input stays open while Caudra works. You can line up the next task, add guidance to the run in progress, or replace it. Running subagents take guidance the same way.

| Key | What happens |
| --- | --- |
| `Enter` | Queues your prompt to run after the current one |
| `Ctrl+X g` | Adds your guidance to the current run before its next model request |
| `Ctrl+X x` | Stops the current run and starts yours |
| `/tasks` | Lists the subagents. Open a running one and type, and it reads your guidance at its next turn boundary |
| `/btw` | Asks a side question without adding it to the conversation |

A subagent's chat opens while the model is still writing its brief, so you can read the task before the subagent starts. When the main agent asks you a question, `F2` opens `/btw` to talk the choices through before you answer.

### Set the finish line and wake to results

`/goal` keeps a session working until a separate evaluator finds evidence that your condition is met, for example "tests pass and clippy is clean". After each work turn, a model call with no tools reads the transcript. If the goal is not met yet, another turn starts. Once it is met, the goal clears itself.

Background tasks and shell jobs report back on their own and wake the agent at a safe boundary, with no polling. Your permission rules decide what may run while you are away. They split shell chains and pipelines into separate commands for approval, and an approval can hold once, for the conversation, for the project, or for every project. Caudra can also notify you when a session finishes or needs your input.

The session has to keep running for this, for example in tmux or [Herdr](https://herdr.dev), and closing it cancels background work. A goal allows 16 automatic continuations by default and pauses whenever it needs your answer. `/goal` looks for evidence in the transcript. It does not prove the work is correct.

### Token savings, built in

Every turn sends the whole conversation again, so a noisy tool result costs tokens on every later turn until compaction. Caudra keeps results small and round trips few.

- Shell output passes through command-aware filters before the model reads it, and they are on by default. Most of the rules come from [RTK](https://www.rtk-ai.app/docs/), and a filtered result is always smaller than the raw one. A failure is never reported as a success, and a failing command keeps its first and last lines. Progress bars collapse to one row. You still watch the raw output while a command runs, and you can switch between the filtered and raw views afterwards.
- `file_index` returns a file outline with signatures and line numbers. The `code_*` tools rank symbols and show callers, impact, and the tests they can trace to a change. They parse the source on the fly, so there is no index to build or maintain and nothing to configure.
- Oversized results reach the model as a bounded head and tail, and `tool_output` searches the rest. `batch` runs independent calls in one turn, and subagents keep their exploration out of the main context.
- Caudra sends a per-conversation cache key wherever a provider accepts one, and `/usage` shows the cache hit rate for every model.

### Open the code without leaving

`Ctrl+X w` opens a workbench with a file explorer, tabbed editor, project search, and source control while the session keeps running. It lives in the terminal, so it comes along over SSH.

Read diffs, browse the commit graph, and stage, unstage, or discard changes per file or folder. `Ctrl+X Enter` sends the file, line, or selection you are looking at to the composer as a mention such as `@src/api.ts:L10-L20`, and Caudra puts those lines in the request.

When a reply needs work in places, `Ctrl+X r` opens passage review. Mark the passages, add a note to each, and send every note back as one prompt. Replies render tables, highlighted code, Unicode maths, and Mermaid flowcharts in the terminal, and copying a passage gives you its Markdown source.

### Never lose the thread

Every session keeps its full history, subagent transcripts included, in compact local storage. The `⋮` menu beside a message lets you fork the session there, or revert the conversation, the files, or both. **Revert files** first shows what would change, and choosing it again applies the revert. **Unrevert** puts the files and the conversation back.

Caudra records file changes before and after each tool call that may change files, so a revert touches only the files those calls changed. It cannot undo external side effects such as running processes, databases, network calls, or Git branch state.

To try another approach on the side, `/worktree new` moves the session into a fresh Git worktree along with its conversation and plan.

What the agent learns can outlast the session too. The `memory` tool keeps tagged project notes, such as gotchas and decisions, in Caudra's state directory rather than in your repository, and every checkout of the repository shares them. Each request carries only the tags until the agent needs a note. `/memory` lists the notes so you can read, edit, or delete them.

### Keep any model on task

Smaller local models such as Qwen3.8-27B need more help to finish a task. Caudra nudges the model when a turn stalls, gets cut off, loops, or stops after announcing work. The rules react to what a reply did, so the same defaults work well with flagship models.

- After an empty or cut-off reply, Caudra asks the model to continue. After a reply such as "I will run the tests now" with no tool call, it asks for the work itself. The third identical tool call in a row is refused before it runs.
- When the model repeats a tool cycle or an answer, or keeps making failed calls, a hint asks it to reconsider its approach. Hints stop at four per run by default and never reopen a finished answer.
- Every rule is on by default, and budgets cap how often each one fires. Under `[agent.steering]` in `caudra.toml`, you can change a threshold, a budget, or the wording of a nudge for all models or for one exact `provider/model-id`.
- Each nudge appears in the transcript as a dim row. Click it to read the exact text the model received.

```toml
# caudra.toml: more patience for one local model
[agent.steering.models."my-server/qwen3.8-27b".rules.abandoned_turn]
max_attempts = 4
```

In daily use, the `abandoned_turn` rule matched 35 of 386 final Qwen3.8-27B replies and none of 2,185 from Claude and GPT models, as [measured by the maintainer](#why-caudra-exists).

A nudge is a message to the model. It cannot run or approve a tool, and every real tool call still passes through validation and your permission rules.

### Tools that say what they missed

When a tool stops at a limit or leaves something out, its result says what is missing, and the model can go back for it. The file, shell, web, code, and Python tools come from [Workcell](https://github.com/caudra/caudra/tree/main/workcell), which also runs on its own as an MCP server.

- A `file_grep` or `file_glob` call that reaches its bounds returns what it found and reports how much it withheld, so the model can tell a missing match from a file it never searched. Directory searches skip credential files such as `.env` and SSH private keys.
- `webfetch` refuses private, loopback, and link-local addresses and checks every redirect the same way. Pages arrive in the character set they declare, and a page cut at 2,000 lines or 50 KiB ends with a line that names the limit.
- A fetched PDF arrives as its text. In attachment mode, Claude through Anthropic or Amazon Bedrock receives the file itself inside the tool result, within a quarter of its context window, and so does a custom model on a compatible API that declares PDF support. Other models receive the extracted text with a line that says why. Saved sessions keep only the URL, name, and page count.
- `python_execution` runs scripts in a separate worker with no file system, network, environment variables, or subprocesses. That isolation is why the default permission policy runs it without a prompt.

## Use the models you already pay for

Caudra has first-class support for Anthropic and OpenAI and ships 16 built-in providers, among them Google, GitHub Copilot, xAI, Mistral, DeepSeek, OpenRouter, Z.AI, Ollama, and llama.cpp. Custom endpoints that speak a supported API work as well.

Nine model jobs decide which model serves each kind of work: Chat, Plan, Subagent, Compact, Title, Goal, Extract, Fast, and Best. Fast is the preferred small model, and session titles, `/goal` checks, and `/extract` run on it until you bind them. Best is the provider's flagship. In `/model`, you can pin any job to a model or let it follow Chat, Plan, Fast, or Best. A local endpoint can name its own Fast and Best models in `providers.toml`.

You can sign in with a ChatGPT subscription, an existing GitHub Copilot sign-in, or an xAI account, or bring API keys. Claude subscription sign-in is experimental, and Anthropic's terms limit Pro and Max subscriptions to official clients.

## Install

On macOS and Linux:

```sh
curl -fsSL https://caudra.ai/install.sh | sh
```

The script installs Caudra to `~/.local/bin`. Set `CAUDRA_INSTALL_DIR` to choose another directory. If you would rather read the script first, download it, look it over, and run it yourself:

```sh
curl -fsSL https://caudra.ai/install.sh -o install.sh
less install.sh
sh install.sh
```

On Windows, in PowerShell:

```powershell
irm https://caudra.ai/install.ps1 | iex
```

This installs to `%LOCALAPPDATA%\caudra` and adds it to your user `PATH`. In Git Bash, the shell script above works too.

With Nix:

```sh
nix run github:caudra/caudra
```

Prebuilt binaries for Linux and macOS on x86_64 and ARM64, and for Windows on x86_64, are on [GitHub Releases](https://github.com/caudra/caudra/releases/latest). To build the main branch yourself:

```sh
cargo install --locked --git https://github.com/caudra/caudra.git caudra
```

A plain `cargo install` build leaves out `python_execution`, the isolated Python tool, because only release builds, Nix, and `just install` from a checkout embed its worker. The other tools work as usual.

## First steps

Connect a provider, then start Caudra in a repository:

```sh
caudra auth login
cd my-project
caudra
```

`caudra auth login` asks for a provider and a sign-in method. Inside Caudra, `/login` offers the same choices.

Caudra opens in Plan mode. There the agent reads and searches freely, writes nothing but its plan, and asks before any command it cannot prove read-only. When the plan looks right, press `Tab` to switch to Build mode. File edits inside the repository then run without asking. Shell commands and web tools still ask first, and you can remember each approval for the conversation or the project.

Instructions you already wrote for other agents carry over. Caudra reads `AGENTS.md` or `CLAUDE.md` from your repository and picks up skills from `.claude` and `.agents` folders.

A few more things to know on day one:

- `F1` lists every key.
- `/docs` opens the whole manual inside Caudra, matched to the version you run.
- `caudra --continue` resumes the most recent session in the current directory.
- `caudra --print` runs non-interactively for scripts and CI and exits when done.
- `caudra acp` lets editors that speak the Agent Client Protocol, such as Zed, drive Caudra.

## Experimental features

Some capabilities are implemented and still experimental. Each stays off until you switch it on in the `[experimental]` table of your global `caudra.toml`, which lives at `~/.config/caudra/caudra.toml`, or at `%APPDATA%\caudra\caudra.toml` on Windows. A project's own settings cannot turn them on.

```toml
[experimental]
workflows = true
```

| Switch | What it adds |
| --- | --- |
| `workflows` | Durable workflows that run subagents in phases, keep a journal, and can pause and resume. They are heavily inspired by [Grok Build workflows](https://x.ai/news/workflows) and mostly compatible with them. |
| `sandboxes` | Managed sandboxes that run workspace tools in a separate VM, while provider credentials and the conversation stay on your machine. They need e2b-libvirt infrastructure that you or your operator run. |
| `decision_engine` | The JEV decision engine and Auto mode. The engine asks an endpoint you configure for typed decisions such as permission advice, content screening, and shell effect predictions. Its predictions add to your permission rules and can miss risks. |
| `cross_session_messaging` | Messages, topics, and shared work between live sessions on one machine. |
| `lua_plugins` | Lua extensions with a Neovim-style API for your own commands, tools, and interface behavior. |
| `remote_workcell` | Workspace tools on a remote Workcell server while the conversation stays on your machine. |

## Privacy

Sessions, retained tool output, and file change records are stored on your machine. Cloud models and network tools still receive what you send them.

Shell commands start from a cleared environment and receive only [a short list of variables](https://caudra.ai/docs/cli/#shell-host-configuration), such as `PATH`, `HOME`, and proxy settings, so API keys set for Caudra stay out of them. `webfetch` checks every URL and redirect before it connects, and refuses private, loopback, and link-local addresses.

There is no tracking. Telemetry stays off unless you send it to a collector you run, and Caudra checks for updates only when you run `caudra update` or turn on the startup check.

Caudra also works offline with a local model. Use Ollama or llama.cpp, or point a `providers.toml` entry at [ninfer-4090](https://github.com/tensorninja/ninfer-4090), the maintainer's custom inference engine for Qwen3.8-27B on a single RTX 4090. Otherwise, a normal run contacts your provider, refreshes the public [models.dev](https://models.dev) catalog at most once a day, and reaches Exa when the agent searches the web.

## Documentation

The manual lives at [caudra.ai/docs](https://caudra.ai/docs/). The same pages ship inside the binary, so `/docs` works offline too. Good places to start:

- [Quick start](https://caudra.ai/docs/quick-start/)
- [Queue and steering](https://caudra.ai/docs/queue/)
- [Automatic steering](https://caudra.ai/docs/configuration/#agent-steering)
- [How Caudra saves tokens](https://caudra.ai/docs/token-economy/)
- [Workbench](https://caudra.ai/docs/workbench/)
- [Sessions, forks, and revert](https://caudra.ai/docs/sessions/)
- [Permissions](https://caudra.ai/docs/permissions/)
- [Providers](https://caudra.ai/docs/providers/)
- [Changes from Maki](https://caudra.ai/docs/changes-from-maki/)

For a complete setup to copy from, see the [example config](https://github.com/caudra/config). [Reference configs](https://caudra.ai/docs/reference-configs/) lists every key of each config file with its default.

## Contributing

Caudra is under active development, and bug reports help a lot. Issues and pull requests are welcome on [GitHub](https://github.com/caudra/caudra/issues). Please read [CONTRIBUTING.md](CONTRIBUTING.md) before you start on a larger change.

Caudra is a Rust workspace, and the `justfile` holds the everyday commands:

```sh
just code-worker   # build the Python worker, once, before the other recipes
just check         # type-check the workspace
just lint          # clippy, with warnings as errors
just test          # the test suite
just ci            # most application CI checks
just install       # install from your checkout, Python worker included
```

Canonical docs live in `docs/content/`, with navigation in `docs/navigation.json` and generated config examples in `docs/examples/`. The pages are compiled into the binary for `/docs`. The website is maintained separately and consumes these same sources. Application development and docs generation do not require JavaScript tooling.

## Credits

Caudra is developed and maintained by [Thorsten Born](https://thorstenborn.com). It stands on the shoulders of [Maki](https://github.com/tontinton/maki) by [Tony Solomonik](https://github.com/tontinton) and has grown in its own direction since the fork. The repository keeps Maki's history and includes work by Maki contributors. Caudra is independently maintained and is not affiliated with or endorsed by the original project.

Caudra builds on ideas from these open-source projects, with thanks:

- [RTK](https://www.rtk-ai.app/docs/): shell output filtering. Most of Workcell's filter rules are copied from RTK under the Apache License 2.0.
- [ripwire](https://github.com/redhat-et/ripwire): code maps
- [Plannotator](https://plannotator.ai/): passage review
- [Herdr](https://herdr.dev): terminal workspaces for agents
- [Grok Build](https://x.ai/news/workflows): durable workflows. The built-in `deep-research` workflow is adapted from Grok Build under the Apache License 2.0.

The file, shell, web, code, and Python tools come from [Workcell](https://github.com/caudra/caudra/tree/main/workcell), released under the Apache License 2.0 and developed in this workspace under `workcell/`. The isolated Python tool runs on [Monty](https://github.com/pydantic/monty) by Pydantic.

## License

Caudra's first-party work is licensed under the [Apache License, Version 2.0](LICENSE) from this transition forward. Maki-derived code and prior MIT-licensed Caudra work retain their [MIT terms](THIRD_PARTY_LICENSES/Maki.txt). Third-party material retains its own MIT, Apache-2.0, or file-level license terms. [NOTICE.md](NOTICE.md) covers the prospective transition, attribution, and third-party exceptions.
