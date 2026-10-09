# Memory

The notes agents saved in this project, oldest first, as one-line summaries inside <memory> tags:

id+n|text   the n entries from id on, summarized (newlines as spaces)

An entry is a note as written under its name (a later note with the same name rewrites it), the deletion of a note, or a note whose text was forgotten. The summaries form a binary tree: each entry is compressed into a line (a short entry is its own line), then adjacent lines are merged in pairs, again and again. So recent lines cover one entry each, and older lines cover more. An entry not summarized yet shows its name and first heading after "(not summarized yet)".

This is your memory of the project, and its latest word on a thing is the truth. When a line bears on your task, zoom into it until you have what you need whole, before you act, guess or ask: `memory` with `command="zoom"` opens line id+n into the two lines it was made from, and n=1 gives the entry whole. `command="search"` finds notes by their words, for what the view does not mention. Notes written after this view was taken arrive in a `# Memory updated` reminder.
