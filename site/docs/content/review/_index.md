+++
title = "Review"
weight = 36
[extra]
group = "Guides"
+++

# Review

Review marks passages of a reply and attaches notes to them, then sends every note back as one prompt. Use it when part of an answer is wrong and the rest is fine, so you can point at the exact lines instead of describing them.

For approving tool calls before they run, see [Permissions](/docs/permissions/). That is a different thing with a similar name.

## Opening

Three ways in:

- `Alt+A` opens the last assistant reply.
- `/review` does the same from the command palette.
- Right-click or long-press any message, then pick **Review passages**. This reaches thinking blocks, tool results, and your own earlier messages.

## Marking a passage

The modal shows the message with a row cursor.

| Key | Action |
|-----|--------|
| `j` `k` | Move the cursor |
| `g` `G` | First or last row |
| `v` | Start extending, press again to collapse |
| `Enter` | Write a note on the marked rows |
| `n` `p` | Jump to the next or previous note |
| `e` `d` | Edit or delete the note under the cursor |
| `Ctrl+S` | Send every note to the prompt |
| `Esc` | Cancel the range, or close |

Dragging with the mouse marks rows too.

A `▌` marks the rows you are about to annotate. A `●` marks rows that already carry a note.

The quote a note captures is the markdown that produced those rows, not the glyphs on screen, so lists and emphasis arrive intact.

## Writing a note

`Enter` opens an editor with the passage above it. Type the comment and press `Ctrl+S` to save, or `Esc` to go back without saving. An empty comment does not save.

You return to the passage view, where you can mark another range and repeat.

## Sending

`Ctrl+S` compiles every note into one block and drops it in the prompt editor as a collapsed paste. Add any extra context, then press Enter to send. Nothing reaches the model until you do, so you can still delete the paste and start over.

The block looks like this:

```
<review>
Address each note on my previous message.

<note>
> HNSW always outperforms IVF-PQ for recall at any scale.
Overstated. Add the memory-budget caveat and cite the benchmark.
</note>

<note surface="tool result">
> {"latency_ms": 1240, "recall@10": 0.91}
Where did this come from? It contradicts the first section.
</note>
</review>
```

The `surface` attribute appears only when the passage came from somewhere other than assistant text.

Once sent, the transcript draws the block as a review card rather than raw tags. Selecting and copying that card gives you the block above, the same way copying rendered markdown gives you its source.

## Notes across messages

Notes collect until you send them. Press `Esc` to leave the modal, open a different message, and keep adding. Maki reminds you how many are waiting.

Starting a new session or loading another one drops pending notes, because the messages they point at are gone.
