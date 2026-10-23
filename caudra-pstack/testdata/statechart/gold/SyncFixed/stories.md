## 1. Offline | Synced

Rule broken: You never see "Synced" while you are offline.
This is the only way to get here in the model.

0. Start: Online | Synced
1. The network drops. Now: Offline | Waiting to sync
2. The server confirms. Now: Offline | Synced

With the fix, step 2 (the server confirms) cannot happen, so you stay at: Offline | Waiting to sync.
