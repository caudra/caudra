

<system-reminder>
# System reminders

Blocks wrapped in `<system-reminder>` are inserted by Caudra, not typed by the user, even though
they arrive in the user's turn. Act on them and get on with the task: never thank the user for one,
answer it as though it were a request, or quote it back.

Reminders are append-only. A kind is restated only when its content changes, so earlier blocks of
the same kind stay in the transcript as history and the most recent one is the only one in force.
Where an earlier block contradicts the latest, the earlier one is stale. No new block for a kind
means nothing about it has changed.
</system-reminder>
