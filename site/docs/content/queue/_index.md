+++
title = "Queue and Steering"
weight = 3
[extra]
group = "Concepts"
+++

# Queue and Steering

The input stays active while Caudra works. Each prompt can wait for another run, guide the current run, or replace it. The queue stacks these as Replacing, Guide, and Up next, one section per group, so its display matches delivery order.

## Send next

Press `Enter` or click `next` while Caudra is working. The prompt waits until the main agent and its background tasks, shell commands, and workflows finish. Pending reports and results still wake the main agent, even with next prompts waiting. The main agent processes those results before the next prompt starts. A final response that leaves background work running does not release the queue.

Queued user input takes priority over automatic `/goal` checks. If no input is waiting, the goal can be checked after the current work and result delivery settle. An [open todo reminder](/docs/context/#open-todo-items) comes before both, as part of the run that is ending.

Queued prompts appear above the input. A section header names each group and counts what is waiting in it, and the sections appear in the order Caudra claims them. Focus the queue with `/queue`, `Enter` on an idle task panel, or a click.

Every row opens with a `⋯` handle. Click it, or press `.` on the focused row, to open the item menu. The menu carries Edit, Move up, Move down, Move to Guide, Move to Up next, Replace current run, Move to Main, and Delete, filtered to what that item allows right now. `Shift+Up` and `Shift+Down` also reorder the focused item. A prompt stays within its Replacing, Guide, or Up next group and cannot cross a compact operation. The same controls reorder live subagent guidance and unsent guidance on completed tasks without crossing between those collections. `Ctrl+Q` removes the first item. Press `g` to move an Up next item to Guide, or `n` to defer guidance to Up next.

Press `b` while the queue is focused or click its mode control to toggle Together. Together sends compatible pending next-run prompts as separate user messages in one model turn. Prompts with different execution modes run in separate turns. The setting resets after those batches are claimed.

## Selecting Plan while work runs

Cycling Build and Plan selects the mode for your next submission. It does not stop running agents or shell jobs. The mode indicator shows the pending transition until the new mode runs. Background reports and results continue to be processed in the current execution mode.

When you submit in Plan while Build work is outstanding, choose:

- **Keep editing** preserves the draft and leaves work running. This is the default, and `Escape` does the same.
- **Queue in Plan** waits for existing work and its result processing to finish, then runs the prompt in Plan. Later mode cycling does not change that queued prompt.
- **Stop work and submit in Plan** cancels and drains the current session work before starting the prompt. Other Up next prompts remain queued.

Work continues while you decide. Stop includes tasks, shell jobs, and workflows, and suppresses late automatic results. It does not undo edits already made. With no conflicting work, a Plan submission starts without this choice.

If stopping fails, the Plan prompt stays blocked. Retry Stop to finish draining the work before the prompt can start.

## Guide the current run

Press `Ctrl+X g` or click `guide` to send the input as guidance. Caudra waits for the current provider response and any tool calls to settle, then adds all waiting guidance before the next model request. An in-flight response is never modified.

A late guide may arrive after the run's final boundary. It then starts before queued next-run prompts instead of being lost. If a replacement is pending, compatible waiting guides enter its first model request before the replacement message. Guidance in a different execution mode waits for its own turn.

When the main agent is waiting for background work, guide wakes it without cancelling the running tasks or commands. Main-chat guidance goes to the main agent, not to every child. Open a task chat to guide that child directly.

## Stop and replace

Press `Ctrl+X x` or click `replace` to cancel the active run and start the input as its replacement. Existing Up next prompts remain queued behind it. Sending another replacement before it starts updates the pending replacement, so the newest one wins.

A prompt that is already waiting can take over the same way. Choose Replace current run from its menu, or press `r` on the focused row. Caudra takes the prompt out of the queue and applies it at once, which cancels the running turn and starts that prompt in its place. When another replacement is already stopping a run, the prompt returns to the group it came from and a message says why.

Deleting a pending replacement turns the operation into a plain cancellation. Caudra waits for the old run to stop before accepting another replacement, while normal Up next prompts can still be queued.

`Esc Esc` stops the main run and all session tasks and workflows, clears the queue, and suppresses automatic continuation from late reports or completion notices. To pick the cancelled turn back up later, use `/continue`, which resumes without adding a message of your own. See [Commands](/docs/commands/#resuming-after-an-interruption).

Replacing a main run also stops and drains its session tasks, shell commands, and workflows before the replacement starts. This applies while the main agent is waiting for background work too. A new user turn re-enables automatic continuation. For task-specific cancellation and promotion, see [Background tasks](/docs/sessions/#background-tasks).

## Recovery

Unclaimed prompts, their delivery modes, and their captured Build or Plan modes are saved with the session. Editing or reordering a prompt keeps its captured mode. A provider or tool error pauses delivery so the next prompt cannot race error recovery. Resume the session to retry or edit the preserved queue.
