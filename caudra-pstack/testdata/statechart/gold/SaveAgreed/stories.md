## 1. Saved, draft

Rule broken: You never see "Saved" while the note is still a draft.
Why: the app stops after one retry and shows Saved anyway.
Listed: SaveAgreedBug.cfg, a config of SaveAgreed.tla, expects TLC to break this rule.
There are 2 ways to get here in the model. This is the shortest.

0. Start: Editing, draft
1. You press Save. Now: Saving, draft
2. The save times out. Now: Saved, draft

In the agreed chart, step 2 (the save times out) cannot happen, so you stay at: Saving, draft.
