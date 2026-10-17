---
title: "Accounts"
description: "Sign in to one provider with several subscriptions and API keys, and let Caudra move between them."
---

An account is one credential for one provider: a subscription login, an API key that `caudra auth login` saved, or a key read from an environment variable. A provider can have several, each with a name such as `personal` or `work`. A conversation uses one account at a time. While the provider's selection is Auto, Caudra moves a conversation to another account before its account runs out.

With one account, nothing changes. The status bar shows no account and the transcript records no switches until a provider has a second one.

## Add an account

```bash
caudra auth login anthropic                       # a first Claude subscription
caudra auth login anthropic --name work           # a second one
caudra auth login anthropic --method api-key --name api
caudra auth status
```

Each login adds an account. Signing in again as an account Caudra already has refreshes its credential and adds nothing. Caudra recognizes the account by the identity the provider reports, or by the key itself.

A new account is named after who it signs in as: the part of its email address before the `@`, or its organization. Without either, it is `default`, then `account-2`, `account-3`, and so on. When the provider already has accounts, Caudra asks you to confirm the name at a terminal. `--name` sets it, and a name that already exists has its credential replaced. A login without `--name` fills the first account that [`providers.toml`](#configuration) lists as signed out.

Claude subscription sign-in is experimental. Anthropic's terms limit Pro and Max subscriptions to official clients. See [Anthropic](/docs/providers/#anthropic).

`/login` in the TUI offers the same choices. For a provider that already has accounts, it asks first whether to add an account or to sign in again as one of them.

`caudra auth status` lists every provider with its selection and strategy, then each account with its kind, its email or plan, the quota left in each window, and whether it is blocked or signed out. `--refresh` asks each subscription for its quota first.

Each credential is a file of its own under the state directory, `auth/accounts/<provider>/<name>.json`, readable only by you. A credential that an earlier version of Caudra saved becomes the provider's `default` account the first time this version starts. MCP, decision endpoint, and Workcell credentials stay where they are.

### Keys from the environment

A provider's key variable, such as `ANTHROPIC_API_KEY`, gives an account named `env`. A variable that holds several keys separated by commas gives `env-1`, `env-2`, and so on, one account per key. Auto treats them like any other accounts. A key that `caudra auth login` saved is tried before the variable.

To give a variable's key a name of its own, list it with `api_key_env`:

```toml
[[anthropic.accounts]]
name = "ci"
api_key_env = "ANTHROPIC_API_KEY"
```

An entry that reads the variable replaces the `env` account.

### Rename and sign out

```bash
caudra auth rename anthropic default personal
caudra auth logout anthropic work
caudra auth logout anthropic --all
```

`logout` deletes the credential and its quota readings. An entry in `providers.toml` stays, listed as signed out, so the next login fills it. The name can be left out when the provider has one signed-in account. Signing out of the pinned account turns Auto back on. An account read from the environment is removed by unsetting its variable.

## Choose the account

Each provider has one selection, saved for every session: `auto`, or one account pinned by name.

```bash
caudra auth use anthropic work     # every conversation uses work
caudra auth use anthropic auto     # let Caudra choose
```

A provider with no saved selection is on Auto. A pinned account is used whatever its quota. When it runs out, the TUI asks what to do, as [In the TUI](#in-the-tui) describes, and `--print` and SDK runs wait for it to refill.

`--account PROVIDER:NAME` or `--account PROVIDER:auto` chooses for one run and saves nothing. Repeat it for more providers. `CAUDRA_ACCOUNT` takes the same values separated by commas, and the flag wins for a provider that both name. Both work in the TUI, `--print`, SDK mode, and `caudra acp`.

## How Auto switches

Providers cache a conversation's prompt per account. The first request on another account sends the whole prompt again at the full input price. Auto therefore keeps a conversation on its account, and moves it only where the move loses little or where staying would fail.

A conversation's cache is cold when the conversation starts, after compaction or a clear, after a model change, and after it sat idle longer than the provider keeps a cache. For a provider that publishes no cache lifetime, Caudra assumes one hour. These moments are the conversation's cold points.

Each account has a reading of the quota left in each of its windows, such as the 5-hour and weekly windows of a Claude or ChatGPT subscription. A window that applies to other models only, such as a weekly Opus limit while you use Sonnet, does not count. The lowest window decides the account's state:

| State | Quota left | What Auto does |
|---|---|---|
| Healthy | `prefer_below` or more | Keeps the conversation on it |
| Low | Less than `prefer_below`, `switch_below` or more | Moves the conversation at its next cold point, when a Healthy account exists |
| Spent | Less than `switch_below` | Moves the conversation at its next request, even with a warm cache |

`prefer_below` is 20 and `switch_below` is 5 unless [`[<provider>.switching]`](/docs/providers/#accounts) sets them. A conversation also moves at its next request when the provider refuses the account with a rate limit longer than 30 seconds, when its sign-in fails, or when it cannot serve the model. A shorter rate limit waits on the same account, which keeps the cache. When every account is Spent and no [key may take over](#pay-per-token-keys), the conversation stays where it is and waits.

A subagent starts on its parent's account while that account is Healthy, so it reuses the prompt the parent already cached. Requests outside a conversation, such as titles, use the account a new conversation would get.

### Strategies

`ordered`, the default, uses the first usable account in priority order: the order of the `providers.toml` entries, then accounts that no entry lists, oldest first, then the provider key and the environment key. Once the first account's quota resets, conversations go back to it at their next cold point.

`paced` compares, in each window, how much time has passed with how much quota is used, and picks the account furthest ahead of its schedule. Quota that is about to reset gets used first. At a cold point, a conversation moves only to an account at least 10 points further ahead. Between equal accounts, the earlier reset wins, then the order.

### Pay-per-token keys

Auto keeps subscription conversations on subscriptions. It moves one onto an API key only when the key's entry sets `fallback = true` and every subscription is Spent. A conversation on a key goes back to a subscription at its next cold point. A pinned key is used whatever `fallback` says.

### Quota readings

Claude and ChatGPT subscriptions report their quota with every response, so the readings stay current without extra requests. Other providers have a reading when they report usage, as `/usage` shows it. When a reading is older than 10 minutes, Caudra asks for a new one in the background, and no request waits for it. An account without a reading counts as Healthy. Running Caudra processes share readings and rate limits, so a second process skips an account the first found spent.

## In the TUI

`/accounts` or `Ctrl+X i` lists the accounts of the current model's provider, with the quota left in each window, such as the 5-hour and weekly windows of a subscription, and when each one resets. Opening it asks every subscription whose reading is more than a minute old for a new one, and the list updates when they answer.

| Key | Action |
|---|---|
| `Enter` | Pin the selected account. On a signed-out account, sign in again |
| `a` | Turn on Auto |
| `n` | Add an account |
| `r` | Rename the account |
| `d` | Sign out of the account and remove its `providers.toml` entry. Press it twice |
| `Ctrl+↑` / `Ctrl+↓` | Move the account in the priority order |
| `s` | Switch between `ordered` and `paced` |
| `Ctrl+R` | Ask each account for a new quota reading |

The order and the strategy are saved to `providers.toml`, which keeps its comments. Moving an account adds entries for the saved accounts the file does not list yet.

With two or more accounts, the status bar shows the account the conversation uses and the quota it has left, as `[auto·personal 41%]`. `auto·` means Auto is selected. The chip takes the warning color below `prefer_below` and the error color below `switch_below`. Click it to open `/accounts`.

Every switch adds a row to the transcript with its cause and its cost:

```text
Switched Anthropic account personal → work: personal weekly 96% used. Prompt cache rebuilds (~84k tokens).
```

When the pinned account is spent, or every account is spent on Auto, a card asks what to do:

- Switch to one of up to three other subscriptions, each shown with the quota it has left.
- Switch and turn on Auto, when an account was pinned.
- Use an API key for this run only, whether or not it sets `fallback`.
- Wait until the account refills, at the reset time the provider reported.
- Choose another account from the full list.

`Esc` puts the card away, and the request keeps waiting until the account refills.

`/usage` lists every account of the provider with its quota, the current one first. Press `g` for lifetime spend, which has a **Per account** section.

## Configuration

Accounts and switching live in the global `providers.toml`, next to the provider's other settings:

```toml
[anthropic.switching]
strategy = "paced"
switch_below = 5
prefer_below = 25
windows = ["5h", "weekly"]

[[anthropic.accounts]]
name = "personal"

[[anthropic.accounts]]
name = "work"

[[anthropic.accounts]]
name = "api"
fallback = true
```

The entries set the priority order and hold what a credential cannot, such as `fallback`. Accounts that `caudra auth login` saved need no entry, and they come after the listed ones. A listed account without a credential shows as signed out, so the same file can move between machines. A table such as `[anthropic]` that holds only `accounts` and `switching` changes nothing else about the built-in provider.

[Accounts in providers.toml](/docs/providers/#accounts) lists every key with its default and range.
