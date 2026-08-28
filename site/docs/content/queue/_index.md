+++
title = "Queue and Steering"
weight = 3
[extra]
group = "Concepts"
+++

# Queue and Steering

The input stays active while Maki works. Each prompt can wait for another run, guide the current run, or replace it. The queue groups these as Replacing, Guide, and Up next so its display matches delivery order.

## Send next

Press `Enter` or click `next` while Maki is working. The prompt stays in the queue until the current run returns control, then starts as the next run. This is the default because it cannot change work already in progress.

Queued prompts appear above the input. Focus the queue with `/queue`, `Enter` on an idle task panel, or a click. You can then edit or delete the item before Maki claims it. `Ctrl+Q` removes the first item. Press `g` to move an Up next item to Guide, or `n` to defer guidance to Up next.

Press `b` while the queue is focused or click its mode control to toggle Together. Together sends the pending next-run prompts as separate user messages in one model turn. The setting resets after that batch is claimed.

## Guide the current run

Press `Alt+S` or click `guide` to send the input as guidance. Maki waits for the current provider response and any tool calls to settle, then adds all waiting guidance before the next model request. An in-flight response is never modified.

A late guide may arrive after the run's final boundary. It then starts before queued next-run prompts instead of being lost. If a replacement is pending, all waiting guides enter its first model request before the replacement message.

## Stop and replace

Press `Alt+X` or click `replace` to cancel the active run and start the input as its replacement. Existing Up next prompts remain queued behind it. Sending another replacement before it starts updates the pending replacement, so the newest one wins.

Deleting a pending replacement turns the operation into a plain cancellation. Maki waits for the old run to stop before accepting another replacement, while normal Up next prompts can still be queued.

`Esc Esc` is different. It cancels the active run and clears its queue.

## Recovery

Unclaimed prompts and their delivery modes are saved with the session. A provider or tool error pauses delivery so the next prompt cannot race error recovery. Resume the session to retry or edit the preserved queue.
