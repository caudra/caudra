# Recordings

A homepage slot shows an illustration until a reviewed terminal recording replaces it. A recording is a real Caudra session in the demo project. It is recorded with asciinema, checked for private data, and published from `public/recordings/` with a poster frame and a manifest entry. Playback keeps the original timing.

Run the `recording:*` scripts from `site/`. `bun run` starts them in `site/`, so a relative path resolves against it.

| Command | What it does |
|---------|--------------|
| `bun run recording:demo <dir>` | Creates the demo project in a new or empty directory outside every Git work tree |
| `bun run recording:check <file> [--id <id>] [--cols <n> --rows <n>]` | Checks a take and prints its manifest entry |
| `bun run recording:poster <id> <seconds>` | Renders the still frame of `public/recordings/<id>.cast` at that time |

## Workflow

1. Prepare the recording account once, as described in [Recording settings](#recording-settings) and the [privacy checklist](#privacy-checklist).
2. Create the demo project in that account's home. The script needs only Bun and Git, so the account can also run `bun scripts/recording-demo.ts` from any checkout it can read.

   ```bash
   bun run recording:demo /home/demo/caudra-recordings/orders
   ```

3. As the recording account, in a terminal at the size the [shot list](#shot-list) gives, record from the demo directory into a raw folder outside this repository. Quitting Caudra ends the take.

   ```bash
   mkdir -p /home/demo/caudra-recordings/raw
   cd /home/demo/caudra-recordings/orders
   asciinema rec --window-size 120x34 --command caudra /home/demo/caudra-recordings/raw/eager.cast
   ```

   Most clips need the original bug. Delete the demo directory and create it again before the next take.
4. Back in `site/`, check the take. The default size is 120x34, so pass the size of a 100x30 clip:

   ```bash
   bun run recording:check /home/demo/caudra-recordings/raw/filter.cast --cols 100 --rows 30
   ```

   The file name supplies the id. Pass `--id filter` when it does not, as with `filter-take3.cast`. Each finding names its event index and time. Errors block and make the command exit non-zero. Re-record instead of editing private data out, because a leak can sit in an escape sequence that the player never draws.
5. Copy the checked take into place under its id:

   ```bash
   mkdir -p public/recordings
   cp /home/demo/caudra-recordings/raw/filter.cast public/recordings/filter.cast
   ```

6. Render the poster at a moment that shows what the clip proves:

   ```bash
   bun run recording:poster filter 21.5
   ```

   The script plays the cast in headless Chromium with the site's asciinema-player and JetBrains Mono, with network requests blocked, and keeps the colours the cast recorded. It screenshots only the terminal, writes `public/recordings/filter.png`, and prints the image size and the manifest entry with that size. It refuses a cast with blocking findings. Install the browser once with `bunx playwright install chromium`.
7. Paste the entry into `defineRecordings({ ... })` in `src/data/recordings.ts`. Replace the TODO title, summary, and alt text, and add `chapters` and `edits` where they apply. See [Manifest entry](#manifest-entry).
8. Run `bun run test` and `bun run check`. The first entry also needs the `ships no footage` case in `tests/unit/recordings.test.ts` updated.

## Recording settings

- **Size.** Record `eager`, `steer`, `workbench`, and `sandbox` at 120x34. Record `btw`, `goal`, `filter`, `review`, `revert`, `nudge`, and `resume` at 100x30. Keep the real terminal at least that large and leave it alone during a take. The checker blocks a take with a resize event.
- **asciinema.** asciinema 3 writes asciicast v3. asciinema 2 writes v2 and takes `--cols 120 --rows 34` and `-c caudra` in place of `--window-size` and `--command`. The checker and the player read both versions.
- **Theme.** Set `theme = "caudra-dark"` under `[ui]` in the recording account's `caudra.toml`. Caudra shows `caudra-light` when the terminal reports light mode, so record in a dark terminal. Set `COLORTERM=truecolor`, or `CAUDRA_TRUECOLOR=1`, so the cast keeps the theme's 24-bit colours instead of the 256-colour fallback.
- **Names.** Use the user `demo`, a neutral host name such as `demo`, and a neutral shell prompt such as `PS1='$ '`. Start Caudra with `--command caudra` so that no shell prompt appears at all. Only the resume and sandbox clips need a shell.
- **Keyboard input.** asciinema does not record keystrokes by default. Never pass `--capture-input`, `-I`, or `--stdin`. The checker blocks a take with input events.
- **Timing.** Keep the original timing and do not pass `--idle-time-limit`. The site player ignores that limit and plays every pause as recorded. When a long wait has to go, shorten that one interval in the cast and describe it in `edits`, for example `A 35 s wait for the model at 0:14 is shortened to 3 s.` A recording's length does not measure performance, so never trim to make Caudra look faster.
- **Takes.** Use a model that follows the prompts reliably, and retake instead of cutting. A take can last at most 120 seconds and weigh at most 3 MiB.
- **Approvals.** In a warm-up session, approve `make check` and `bun test` with `Yes, and always allow ‹scope› in this project`, and pick the pattern ending in `*` when one is offered. Stored rules live in Caudra's state, so the demo repository stays clean. File edits inside the project need no approval.
- **Mode.** Caudra opens in Plan mode. Press `Tab` before the first prompt of a clip in which the agent edits files.

## Privacy checklist

- [ ] Record as a dedicated OS account or container user named `demo`, with home `/home/demo`. The checker allows the home paths of `demo` and blocks every other user's.
- [ ] Give that account a fresh Caudra config and state. Otherwise your own sessions, stash, memory notes, plans, MCP servers, custom commands, and workflows appear in pickers and completions.
- [ ] Sign in to the model provider before recording. Keep `/auth`, `.env`, `providers.toml`, and anything else that shows credentials off screen.
- [ ] Record in the demo project only. Its commits use `Demo <demo@example.com>`. The checker allows the local part `git` and the domains `example.com`, `example.net`, `example.org`, `.example`, `.test`, `.invalid`, and `localhost`. It blocks every other email address.
- [ ] Copy nothing during a take. Without a system clipboard, as over SSH, Caudra copies through an OSC 52 clipboard write. The copied text then lands in the cast, and the checker blocks it. That covers `Ctrl+Y` in `/btw`, `Ctrl+X y`, and `Ctrl+C` on a selection.
- [ ] Run no `!` command that prints environment variables, config files, or anything outside the demo project.
- [ ] Review every hyperlink warning. Caudra draws Markdown links as OSC 8 hyperlinks, and playback hides their targets, so confirm that each target is public.
- [ ] Watch each take once in full with `asciinema play` before checking it. The checker finds patterns. You find what it cannot, such as a real name in a model reply.
- [ ] Keep raw takes outside the repository, and delete the rejected ones.

## The demo project

`bun run recording:demo <dir>` refuses a path inside this repository, inside any other Git work tree, or in a directory that is not empty. It writes a Bun project without dependencies:

| Path | Content |
|------|---------|
| `src/pagination.ts` | Cursor pagination. `paginate()` returns a `nextCursor` whenever a page has items, so the final page gets one too |
| `src/pagination.test.ts`, `src/orders.test.ts` | Seven tests. Three fail: the final page, a last page that is exactly full, and a walk over 240 orders that ends with an empty sixth page |
| `src/orders.ts`, `fixtures/orders.json` | `listOrders(limit, cursor)` over 240 seeded orders |
| `scripts/seed.ts` | Rewrites the fixture and prints one progress line per 10 orders |
| `Makefile` | `make check` runs `$(MAKE) seed`, then `$(MAKE) test` |
| `AGENTS.md`, `README.md` | Notes for the agent and the reader |

The script runs `git init` on branch `main` and makes one commit as `Demo <demo@example.com>` with a fixed date. It also writes that identity to the repository config, so commits made during a take use it.

`make check` is the noisy command. Workcell's shell output filter reduces it in two stages, and the shell card footer reads `filtered · make, progress`:

- **`make`** is the built-in rule for `make`. It drops the `make[1]: Entering directory` and `make[1]: Leaving directory` lines that the two sub-makes print.
- **`progress`** collapses the 24 progress frames, which look like `seed orders [==========          ] 120/240  50%`, into `... (23 progress updates collapsed)` and the final frame.

The test failures arrive on stderr and pass through unchanged. An `escapes` stage comes first when the environment forces colour. A rule applies only when the request is a single command, so a pipeline such as `make check | tail -40` loses the `make` stage. The stages come from the Workcell revision pinned in the root `Cargo.toml`, with the rule in `crates/output-filter/rules/make.toml`. Run `make check` through Caudra again after a Workcell bump.

## Shot list

Placement names a story and clip id in `src/data/home.ts`.

| Priority | Id | Placement | Size | Length |
|----------|----|-----------|------|--------|
| First | `eager` | Homepage hero, `heroClip` | 120x34 | 30 to 45 s |
| First | `steer` | Story 01 `steer`, first clip | 120x34 | 60 to 90 s |
| First | `workbench` | Story 04 `workbench`, first clip | 120x34 | 45 to 60 s |
| First | `filter` | Story 03 `savings` | 100x30 | 30 to 45 s |
| Then | `btw` | Story 01 `steer`, second clip | 100x30 | 30 to 45 s |
| Then | `goal` | Story 02 `sleep` | 100x30 | 60 to 120 s |
| Then | `review` | Story 04 `workbench`, second clip | 100x30 | 30 to 45 s |
| Then | `revert` | Story 05 `thread` | 100x30 | 45 to 60 s |
| Then | `nudge` | Story 06 `nudges` | 100x30 | 30 to 45 s |
| Later | `resume` | Story 05 `thread`, new slot | 100x30 | 20 to 30 s |
| Later | `sandbox` | New slot, experimental | 120x34 | 45 to 60 s |

Each clip starts in a fresh demo project and a new session unless its setup says otherwise. Send each quoted prompt with `Enter`.

### eager

1. Send `In one reply, without waiting for results: grep for nextCursor, read src/pagination.ts, and start a task that runs bun test and lists the failing tests.`
2. While the reply streams, click the streaming `task` call. Its chat opens and the brief grows as the model writes it.
3. Click `[< Main]` in the task's status bar and let the reply finish.

Retake until the grep, the read, and the task arrive in one reply.

- **Proves:** each call starts once its own arguments are complete, while the same reply is still streaming. A task's chat opens before its brief is finished.
- **Must not imply:** that a call starts before its arguments are complete, that eager dispatch skips permission checks, or that the clip's length measures speed.

### steer

Setup: Build mode.

1. Send `Fix the final-page cursor bug in src/pagination.ts. Start a task that adds a regression test for an empty listing to src/pagination.test.ts while you work on the fix.`
2. While the main agent works, type `Keep nextCursor null on the last page rather than leaving the field out.` and press `Ctrl+X g`. The guidance joins the run before its next model request.
3. Press `Ctrl+X a`, select the running task, and press `Enter`. Type `Call the new test "an empty listing has no next cursor".` and press `Enter`. The guidance stays above the input until the subagent reads it at its next turn boundary.
4. Click `[< Main]`, type `Then run make check.`, and press `Enter`. The prompt waits under Up next until the run and its background work finish.

Leave out `Ctrl+X x`, because replacing the run would also stop the task this clip is about.

- **Proves:** the input stays open while work runs. Guidance reaches the main run and a running subagent at their next boundary, and `Enter` queues a prompt for after the run.
- **Must not imply:** that guidance changes a response that is already streaming, or that guidance in the main chat reaches subagents.

### workbench

Setup: continue the steer session, or any session in which the agent fixed `src/pagination.ts` and left the change uncommitted.

1. Send `Run make check.` and press `Ctrl+X w` while it runs.
2. Press `Ctrl+X 2`, move to `src/pagination.ts` under `CHANGES`, and press `Enter` to open its diff.
3. Press `Ctrl+P`, type `orders.test`, and press `Enter`.
4. Press `Ctrl+G`, type `11`, and press `Enter`. Press `Shift+↓` four times and then `Shift+End` to select the loop on lines 11 to 15.
5. Press `Ctrl+X Enter`. The workbench closes, and the composer holds `@src/orders.test.ts:L11-L15`.
6. Type ` Why did this loop end with an empty page before the fix?` after the mention and press `Enter`.

- **Proves:** the workbench opens beside a working session, shows the agent's change as a diff, and sends exact lines as a mention that Caudra puts in the request.
- **Must not imply:** language-server features, that a mention sends the whole file, or that the workbench needs more than the terminal.

### filter

1. Send `Run make check and tell me which tests fail. Do not change any files.`
2. The shell card streams the raw output. When the command ends, the card switches to the filtered view with the footer `filtered · make, progress · N% smaller · click for raw`.
3. Click `click for raw`, then click `click for filtered`.

Retake when the agent adds a pipe, because the `make` stage then does not apply.

- **Proves:** completed output shrinks before the model reads it, the failures survive, the footer names each stage, and the raw output is one click away.
- **Must not imply:** that a model summarises the output, that this percentage is typical, that the raw capture is discarded, or that filtering changes the command.

### btw

1. Send `Before you change any code, use the question tool to ask me whether the last page should return nextCursor: null or leave the field out.`
2. When the question form appears, press `F2`.
3. Type `Which option keeps clients that check for null working?`, press `Enter`, and let the answer finish.
4. Press `Esc`. The form returns unchanged.
5. Move to the `null` option with `↑` or `↓` and press `Enter`.

- **Proves:** a side question while the agent waits for an answer, a form that is unchanged after `Esc`, and a side thread that stays out of history.
- **Must not imply:** that `/btw` runs tools or reads files, that its answer enters the conversation, or that subagent questions support it.

### goal

Setup: Build mode, so that the goal runs without approval prompts.

1. Send `/goal make check exits 0 and no existing test was edited`
2. Let the work turn finish. The evaluator runs once the turn and its background work settle.
3. Click the goal indicator in the footer to open the panel with the condition, evaluations, and continuation limit. Close it with `Esc`.
4. End the take when the goal reports that it is met and clears itself.

- **Proves:** after each work turn, a separate evaluator looks for evidence in the transcript. An unmet goal starts another turn, and a met goal clears itself.
- **Must not imply:** that a met goal proves correctness, that work continues after the session closes, that continuations are unlimited (16 by default), or that a goal bypasses permissions.

### review

Setup: continue a session in which the agent fixed the bug.

1. Send `Explain the bug and the fix in four bullet points.`
2. Press `Ctrl+X r` to open the reply.
3. Move to a bullet with `↓` and extend over it with `Shift+↓`. Press `Enter`, type `Say why null is safer than leaving the field out.`, and press `Ctrl+S` to save the note.
4. Mark another bullet the same way, write `Name the regression test.`, and save it.
5. Press `Ctrl+S` to send both notes to the composer as one collapsed paste, then press `Enter`.

- **Proves:** notes attach to exact passages and leave as one prompt, and nothing reaches the model until `Enter`.
- **Must not imply:** tool-call approval, which is a separate feature in Permissions, or a review of diffs.

### revert

Setup: continue a session in which one prompt led the agent to edit `src/pagination.ts` and `src/pagination.test.ts`, with no work running.

1. Send `!git diff --stat` to list the two changed files.
2. Click `⋮` beside that prompt and choose **Revert files**. The notice counts the files it would create, replace, or delete.
3. Choose **Revert files** again to apply it, then send `!git diff --stat` again. It prints nothing.
4. Open the menu again and choose **Unrevert**. The files and the conversation head come back.

- **Proves:** the first Revert files only previews, and the second applies. Only files that the session's tool calls changed are touched, and Unrevert puts them back.
- **Must not imply:** that revert undoes processes, databases, network calls, or Git branch state, that it restores files you edited yourself, or that it takes snapshots of the disk.

### nudge

Setup: Build mode, so that `make check` runs without an approval prompt.

1. Send `Answer with only this sentence: I will run make check now.`
2. The reply ends on that sentence and calls no tool. A dim row appears below it, and the agent runs `make check`.
3. Click the dim row to show the exact text the model received, then click it again to fold it.

Retake when the first reply calls a tool, or adds a question or a summary after the sentence.

- **Proves:** a reply that announces work and calls no tool gets one short nudge, the transcript shows its exact text, and the work that follows goes through the normal permission rules.
- **Must not imply:** that the model stopped early on its own, because the prompt asks for the announcement. It must also not suggest that a nudge runs or approves a tool, or that steering checks the work.

### resume

Setup: record a shell with a neutral prompt in the demo directory. The most recent session there should hold a task transcript, as the steer session does.

1. Run `caudra --continue`.
2. Scroll up through the restored transcript.
3. Press `Ctrl+X a` and open the task to show that its transcript came back too.
4. Quit with `Ctrl+C`.

- **Proves:** a session reopens with its full history, subagent transcripts included.
- **Must not imply:** that running work survives quitting, because closing a session cancels its background work. It must also not suggest that the demo's load time stands for large sessions.

### sandbox

Setup: `sandboxes = true` under `[experimental]`, a provider and profile that the operator prepared, and a sandbox named `dev` created off camera with the demo project in its workspace. Keep provider endpoints, instance IDs, and lifecycle keys off screen, and never run `caudra auth sandbox` during a take.

1. In a shell with a neutral prompt, run `caudra --sandbox dev`.
2. Send `Run uname -a and bun test, and say where they ran.`
3. Run `/sandbox` to show the live sandbox, then close the manager with `Esc`.

- **Proves:** workspace tools run in the VM while the interface, credentials, and conversation stay on the client, and attaching to a sandbox is explicit.
- **Must not imply:** that sandboxes are stable or on by default, that a fresh run uses one, or that Caudra creates a VM without being asked.

## Manifest entry

Each key in `defineRecordings({ ... })` in `src/data/recordings.ts` names a slot. `Clip.astro` renders every clip in `src/data/home.ts` as `<Recording id="...">`, and an entry with the same id replaces that clip's illustration with the poster and player. The page itself needs no edit.

Two clips have no slot yet. `resume` needs a new clip with the id `resume` in the `thread` story, with an illustration diagram that shows until the recording exists. `sandbox` needs a new homepage slot, because docs pages are plain Markdown compiled into the binary and cannot host the component.

| Field | Content |
|-------|---------|
| `title`, `summary` | What the clip shows. The caption displays both |
| `poster.alt` | What the still frame shows |
| `cols`, `rows`, `duration` | From the checker. The duration is rounded up to whole seconds |
| `chapters` | Optional `{ title, time }` markers in seconds, in order and inside the duration |
| `maturity` | `stable`, or `experimental` for a feature behind an experimental switch, such as `sandbox` |
| `edits` | Every change to the original timing or content. The caption displays it |
