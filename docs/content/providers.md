---
title: "Providers"
description: "Model catalogs, env vars, providers.toml, model jobs."
---

Caudra talks to LLM providers over their HTTP APIs. Model jobs decide which configured or discovered model serves each kind of work.

## Model jobs

Caudra routes work through nine jobs: **Chat**, **Plan**, **Subagent**, **Compact**, **Title**, **Goal**, **Extract**, **Fast**, and **Best**. A global binding can pin a job to an exact `provider/model-id` or make it follow Chat, Plan, Fast, or Best. Explicit bindings report an error when their model is unavailable or disallowed.

`/model` opens a Jobs overview and the model list. Selecting a model on this page changes Chat. Selecting the Chat row jumps to its current model. Select another job to open its assignment page, press `Esc` to return to the overview, and use uppercase `R` to clear the open job's binding. `/goal-model` opens Goal directly. Jobs are opened from the overview rather than cycled with `Tab`.

Bindings are saved globally in the `model.purposes` row of Caudra's SQLite state database and apply across sessions. Unbound jobs use these rules:

| Job | Default when unbound |
|-----|----------------------|
| Chat | The anchor model |
| Plan | The anchor model |
| Subagent | The model currently running its parent |
| Compact | The model currently running the caller |
| Title | Fast |
| Goal | Fast |
| Extract | Fast |
| Fast | Provider `fast` config, curated preferred small model, cheapest priced model, fewest-parameter model, then the anchor |
| Best | Provider `best` config, curated flagship, then the anchor |

The anchor is the selected Chat model when a main turn starts. A Plan binding can select a distinct model, which Caudra uses for main turns sent in Plan mode. An explicit global Subagent binding overrides parent inheritance. A prompt profile's `subagent_model` overrides the global Subagent binding for tasks using that profile. See [System Prompt Profiles](/docs/system-prompts/#configure-subagents).

See [Sessions](/docs/sessions/#titles) for Title, [Completion goals](/docs/commands/#completion-goals) for Goal, and [Requirements](/docs/commands/#requirements) for Extract.

## Supply metadata

Provider catalogs record whether a model is small and whether it is the preferred model in its size lane. The tables below and `caudra models` render those facts with three markers:

| Marker | Meaning |
|--------|---------|
| Small | A small alternative |
| Fast | The preferred small model |
| Best | The provider's flagship |

A known non-small alternative and a model with no supply facts both have no marker. Markers describe provider supply. They are not capability tiers, and price or list order never creates one. Fast may still use price or parameter count as its final provider fallback without adding a marker.

Aggregators such as OpenRouter borrow supply facts from the upstream vendor entry in a vendor-prefixed model id. Tool deferral also uses the small fact: small and unknown models defer on-demand tools, while known non-small models receive them upfront. See [Tools loaded on demand](/docs/tools/#which-models-defer).

Provider `purposes` entries in `providers.toml` define the same supply facts. `fast` entries are small and `best` entries are non-small. The first entry in each list is preferred, so it receives the Fast or Best marker. Later `fast` entries receive Small, while later `best` entries remain unmarked. Entries match by prefix, so an endpoint serving a family of fine-tunes needs one line rather than one per variant:

```toml
[my-server.purposes]
fast = "qwen3.8-27b"   # covers qwen3.8-27b, qwen3.8-27b-canary, qwen3.8-27b-math7, ...
```

The first entry also names the model that wins automatic resolution, so it has to name a model the endpoint actually serves. A prefix that matches nothing live would send requests to an id that does not exist.

## Auth Reloading

Caudra re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `caudra auth login` in another terminal or change an env var, the next session picks it up without a restart.

You can set multiple API keys in one env var (`ANTHROPIC_API_KEY=sk-1,sk-2,sk-3`) and they rotate automatically on rate-limit or auth errors.

## Base URL Overrides

Most providers, custom ones included, honor a `<SLUG>_BASE_URL` env var, where `<SLUG>` is the slug in capitals with `_` for `-` (`anthropic` -> `ANTHROPIC_BASE_URL`, `openrouter` -> `OPENROUTER_BASE_URL`). Set it to the origin of a proxy or a compatible endpoint and Caudra appends the API paths itself:

```sh
ANTHROPIC_BASE_URL=https://my-proxy.internal caudra
```

It wins over `providers.toml` and built-in defaults. `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` are the same names the official SDKs use, so an existing proxy setup carries over as is. Three exceptions apply. `ANTHROPIC_BASE_URL` never receives Claude subscription tokens. `OPENAI_BASE_URL` only redirects the platform API, never the ChatGPT Coding Plan backend. `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth CLI proxy.

You can also set `base_url` for a built-in provider in `~/.config/caudra/providers.toml`. It overrides the built-in default and loses to the env var above:

```toml
[openai]
base_url = "http://xxxx:1234/v1"
```

The built-in provider still owns the slug, so `protocol`, `api_key_env`, `discover_models` and `models` are ignored with a warning. Use a custom slug if you need those.

Ollama reads `OLLAMA_HOST` and llama.cpp reads `LLAMA_CPP_HOST`. Neither reads `<SLUG>_BASE_URL`. A `base_url` in `providers.toml` wins over the host variable, and Caudra appends `/v1` to either, so leave it off. Aperture reads `APERTURE_HOST`, which wins over the file. Copilot asks GitHub for the API endpoint of your account and ignores both settings.

## Built-in Providers

### Anthropic

- **Env var**: `ANTHROPIC_API_KEY` (also supports subscription OAuth via `caudra auth login anthropic`)
- **API**: `https://api.anthropic.com/v1/messages`
- **Features**: Prompt caching, thinking mode (adaptive/budgeted), advanced tool use

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Fast | **claude-haiku-4-5** (default) | $1.00 / $5.00 | 200K ctx / 64K out |
|  | claude-sonnet-4-5 | $3.00 / $15.00 | 200K ctx / 64K out |
|  | claude-sonnet-4-6 | $3.00 / $15.00 | 372K ctx / 64K out |
|  | claude-sonnet-5-5 | $2.00 / $10.00 | 372K ctx / 128K out |
|  | claude-sonnet-5 | $2.00 / $10.00 | 372K ctx / 128K out |
|  | claude-sonnet-4 | $3.00 / $15.00 | 200K ctx / 64K out |
|  | claude-opus-4-5 | $5.00 / $25.00 | 200K ctx / 64K out |
|  | claude-opus-4-6 | $5.00 / $25.00 | 372K ctx / 128K out |
|  | claude-opus-4-7 | $5.00 / $25.00 | 372K ctx / 128K out |
|  | claude-opus-4-8 | $5.00 / $25.00 | 372K ctx / 128K out |
| Best | **claude-opus-5-5** (default) | $4.00 / $20.00 | 372K ctx / 128K out |
|  | claude-opus-5 | $5.00 / $25.00 | 372K ctx / 128K out |
|  | claude-fable-5-1 | $10.00 / $50.00 | 372K ctx / 128K out |
|  | claude-fable-5 | $10.00 / $50.00 | 372K ctx / 128K out |
|  | claude-opus-4-0, claude-opus-4-1 | $15.00 / $75.00 | 200K ctx / 32K out |

Routing defaults: claude-haiku-4-5 (Fast), claude-opus-5-5 (Best)

Run `caudra auth login anthropic` to sign in to a Claude subscription through browser OAuth. Caudra stores the tokens in its state directory, refreshes them automatically, and shows subscription limits through `/usage`. Subscription requests always go to `api.anthropic.com`, even when `ANTHROPIC_BASE_URL` is set.

This experimental flow uses Claude Code's public client registration. Anthropic limits Pro and Max subscription tokens to official clients in its terms. The flow may stop working when Anthropic changes its OAuth or request protocol.

Recent Claude models accept up to 1M tokens. Caudra runs them at a 372k working window, which keeps cost and latency bounded. That window is an input budget: the model's output allowance sits on top of it rather than inside it, so Caudra holds back less of it before compaction. Add `-1m` to a model id, like `claude-sonnet-4-6-1m`, to open the full 1M window instead. Set `context_window` in `providers.toml` to pick any other size.

#### Amazon Bedrock

If you already use Claude through AWS Bedrock, you can point Caudra at it instead of the direct Anthropic API. Set `CLAUDE_CODE_USE_BEDROCK=1` and Caudra will route all Anthropic requests through Bedrock. The same models, the same features, just a different door.

You will need `AWS_REGION` and one of the following for auth:

| Method | Env vars |
|--------|----------|
| IAM credentials | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (and optionally `AWS_SESSION_TOKEN`) |
| Credentials file | `AWS_PROFILE` (defaults to `default`), reads `~/.aws/credentials` |
| Bearer token | `AWS_BEARER_TOKEN_BEDROCK` |
| Gateway proxy | `CLAUDE_CODE_SKIP_BEDROCK_AUTH=1` + `ANTHROPIC_BEDROCK_BASE_URL` (skips signing, useful behind a proxy that handles auth) |

You can override the model with `ANTHROPIC_MODEL` and the endpoint with `ANTHROPIC_BEDROCK_BASE_URL`. These env var names match Claude Code, so if you were already using Bedrock there, the same setup works here.

### OpenAI

- **Env var**: `OPENAI_API_KEY` (also supports OAuth device flow)
- **API**: `https://api.openai.com/v1`

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Fast | **gpt-6-luna** (gpt-6 default) | $0.10 / $0.50 | 372K ctx / 128K out |
| Fast | **gpt-5.6-luna** (gpt-5.6 default) | $0.20 / $1.20 | 372K ctx / 128K out |
| Small | gpt-5.4-nano | $0.20 / $1.25 | 400K ctx / 128K out |
| Small | gpt-5.4-mini | $0.75 / $4.50 | 400K ctx / 128K out |
| Small | gpt-4.1-nano | $0.10 / $0.40 | 1047K ctx / 32K out |
| Best | **gpt-6-astra** (gpt-6 default) | $10.00 / $50.00 | 372K ctx / 128K out |
|  | gpt-6.1-sol | $2.00 / $10.00 | 372K ctx / 128K out |
|  | gpt-6-sol | $2.00 / $10.00 | 372K ctx / 128K out |
|  | gpt-5.6-terra | $2.00 / $12.00 | 372K ctx / 128K out |
| Best | **gpt-5.6-sol** (gpt-5.6 default) | $4.00 / $20.00 | 372K ctx / 128K out |
|  | gpt-4.1-mini | $0.40 / $1.60 | 1047K ctx / 32K out |
|  | gpt-4.1 | $2.00 / $8.00 | 1047K ctx / 32K out |
|  | o4-mini | $1.10 / $4.40 | 200K ctx / 100K out |
|  | gpt-5.5 | $5.00 / $30.00 | 1050K ctx / 128K out |
|  | gpt-5.4 | $2.50 / $15.00 | 1050K ctx / 128K out |
|  | o3 | $2.00 / $8.00 | 200K ctx / 100K out |
|  | gpt-5.3-codex | $1.75 / $14.00 | 400K ctx / 128K out |
|  | gpt-5.2-codex | $1.75 / $14.00 | 400K ctx / 128K out |
|  | gpt-5.2 | $1.75 / $14.00 | 400K ctx / 128K out |
|  | gpt-5.1-codex-mini | $0.25 / $2.00 | 400K ctx / 128K out |
|  | gpt-5.1-codex-max | $1.25 / $10.00 | 400K ctx / 128K out |
|  | gpt-5.1-codex | $1.25 / $10.00 | 400K ctx / 128K out |

Routing defaults: gpt-6-luna (Fast), gpt-6-astra (Best)

A lane answers inside the release line you are on. On gpt-6 that is gpt-6-luna (Fast) and gpt-6-astra (Best). On gpt-5.6 that is gpt-5.6-luna (Fast) and gpt-5.6-sol (Best).

### Google

- **Env var**: `GEMINI_API_KEY`
- **API**: `https://generativelanguage.googleapis.com/v1beta`
- **Features**: Native Gemini API with thinking support

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Fast | **gemini-2.0-flash-lite** (default) | $0.07 / $0.30 | 1048K ctx / 65K out |
| Best | **gemini-2.5-pro** (default) | $1.25 / $10.00 | 1048K ctx / 65K out |
|  | gemini-2.5-flash | $0.30 / $2.50 | 1048K ctx / 65K out |

Routing defaults: gemini-2.0-flash-lite (Fast), gemini-2.5-pro (Best)

### Copilot

- **Env var**: `GH_COPILOT_TOKEN` (or run `caudra auth login copilot` to import a token from gh CLI, the Copilot client, or the system keyring)
- **API**: `https://api.githubcopilot.com (or GraphQL-discovered Copilot API endpoint)`
- **Features**: Native Copilot Chat HTTP API with model endpoint discovery

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Small | gpt-5-mini | $0.25 / $2.00 | 200K ctx / 100K out |
| Small | gpt-5.4-mini | $0.75 / $4.50 | 200K ctx / 100K out |
| Small | gpt-5.4-nano | $0.20 / $1.25 | 200K ctx / 100K out |
| Small | claude-haiku-4.5 | $1.00 / $5.00 | 200K ctx / 64K out |
| Small | gemini-3.5-flash | $1.50 / $9.00 | 200K ctx / 65K out |
| Small | mai-code-1-flash-picker | $0.75 / $4.50 | 200K ctx / 100K out |
| Small | gpt-6-luna | $0.10 / $0.50 | 200K ctx / 100K out |
| Fast | **gpt-5.6-luna** (default) | $0.20 / $1.20 | 200K ctx / 100K out |
|  | gemini-3.6-flash | $0.75 / $3.75 | 200K ctx / 65K out |
|  | gemini-3.7-flash | $0.75 / $3.75 | 200K ctx / 65K out |
|  | claude-sonnet-4.5, claude-sonnet-4.6 | $3.00 / $15.00 | 200K ctx / 64K out |
|  | claude-sonnet-5.5 | $2.00 / $10.00 | 200K ctx / 100K out |
|  | claude-sonnet-5 | $2.00 / $10.00 | 200K ctx / 100K out |
|  | gpt-5.5 | $5.00 / $30.00 | 200K ctx / 100K out |
|  | kimi-k2.7-code | $0.95 / $4.00 | 200K ctx / 100K out |
|  | kimi-k3 | $3.00 / $15.00 | 200K ctx / 100K out |
|  | gemini-3.1-pro-preview | $2.00 / $12.00 | 200K ctx / 65K out |
|  | gpt-6.1-sol | $2.00 / $10.00 | 200K ctx / 100K out |
|  | gpt-6-sol | $2.00 / $10.00 | 200K ctx / 100K out |
|  | gpt-5.4 | $2.50 / $15.00 | 200K ctx / 100K out |
|  | gpt-5.6-sol | $4.00 / $20.00 | 200K ctx / 100K out |
|  | gpt-5.6-terra | $2.00 / $12.00 | 200K ctx / 100K out |
|  | gpt-5.3-codex | $1.75 / $14.00 | 200K ctx / 100K out |
|  | claude-opus-5.5 | $4.00 / $20.00 | 200K ctx / 128K out |
| Best | **claude-opus-5, claude-opus-4.8, claude-opus-4.7, claude-opus-4.6, claude-opus-4.5** (default) | $5.00 / $25.00 | 200K ctx / 64K out |
|  | claude-opus-4.8-fast, claude-fable-5 | $10.00 / $50.00 | 200K ctx / 100K out |
|  | grok-4.5 | $2.00 / $6.00 | 200K ctx / 100K out |
|  | grok-4.6 | $2.00 / $6.00 | 200K ctx / 100K out |

Routing defaults: gpt-5.6-luna (Fast), claude-opus-5 (Best)

### Ollama

- **Env var**: `OLLAMA_HOST` for local/remote (e.g. `http://localhost:11434`), `OLLAMA_API_KEY` for auth
- **API**: `http://localhost:11434/v1`
- **Features**: Local or remote inference via OLLAMA_HOST, cloud fallback via OLLAMA_API_KEY

This provider talks the OpenAI-compatible `/v1` API, so it also works with llama.cpp's server, LocalAI, or anything else that speaks the same protocol. Just point `OLLAMA_HOST` to the right address (e.g. `http://localhost:8080` for llama.cpp).

### LlamaCpp

- **Env var**: `LLAMA_CPP_API_KEY`
- **API**: `http://localhost:8080/v1`
- **Features**: Local or remote inference via LLAMA_CPP_HOST, set optional key via LLAMA_CPP_API_KEY

Connects to any OpenAI-compatible `/v1` endpoint. Set `LLAMA_CPP_HOST` to your server address, such as `http://localhost:8080`, or run `caudra auth login llama-cpp`, which offers that address and saves your answer to `providers.toml`. Without either, Caudra reports that `LLAMA_CPP_HOST` is not set.

### Mistral

- **Env var**: `MISTRAL_API_KEY`
- **API**: `https://api.mistral.ai/v1`

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Fast | **ministral-14b-latest, ministral-14b-2512** (default) | $0.20 / $0.20 | 262K ctx |
| Best | **mistral-medium-latest, mistral-medium-3.5, mistral-medium-3-5, mistral-medium-2604** (default) | $1.50 / $7.50 | 262K ctx |
|  | glm-5-2, zai-glm-5-2 | $1.40 / $4.40 | 1000K ctx |
|  | mistral-small-latest, mistral-small-2603 | $0.15 / $0.60 | 262K ctx |

Routing defaults: ministral-14b-latest (Fast), mistral-medium-latest (Best)

### Z.AI

- **Env var**: `ZHIPU_API_KEY` (shared across both endpoints)
- **API endpoints**:
  - `https://api.z.ai/api/paas/v4`
  - `https://api.z.ai/api/coding/paas/v4`

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Fast | **glm-4.7-flash** (default) | $0.00 / $0.00 | 200K ctx / 131K out |
| Small | glm-4.5-flash | $0.00 / $0.00 | 131K ctx / 98K out |
| Small | glm-4.5-air | $0.20 / $1.10 | 131K ctx / 98K out |
| Best | **glm-5-code** (default) | $1.20 / $5.00 | 200K ctx / 131K out |
|  | glm-5.2 | $1.00 / $3.20 | 1000K ctx / 131K out |
|  | glm-5.1, glm-5 | $1.00 / $3.20 | 200K ctx / 131K out |
|  | glm-4.7, glm-4.6 | $0.60 / $2.20 | 200K ctx / 131K out |
|  | glm-4.5 | $0.60 / $2.20 | 131K ctx / 98K out |

Routing defaults: glm-4.7-flash (Fast), glm-5-code (Best)

### DeepSeek

- **Env var**: `DEEPSEEK_API_KEY`
- **API**: `https://api.deepseek.com`
- **Features**: Thinking mode toggle (on/off), open-weight models
- **Peak pricing**: the prices below are off-peak; each turn is billed as it happens, at 2x during 01:00-04:00, 06:00-10:00 UTC

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
|  | deepseek-v4-flash | $0.22 / $0.66 | 1000K ctx / 384K out |
| Best | **deepseek-v4-pro** (default) | $0.66 / $1.98 | 1000K ctx / 384K out |

Routing defaults: deepseek-v4-pro (Best)

### OpenRouter

- **Env var**: `OPENROUTER_API_KEY`
- **API**: `https://openrouter.ai/api/v1`
- **Features**: 300+ models from all providers, prompt caching, provider routing

OpenRouter aggregates models from many providers behind a single API key. Browse available models at [openrouter.ai/models](https://openrouter.ai/models). Use any model ID directly (e.g. `openrouter/anthropic/claude-sonnet-4`).

### Synthetic

- **Env var**: `SYNTHETIC_API_KEY`
- **API**: `https://api.synthetic.new/openai/v1`
- **Features**: Reasoning effort support (low/medium/high), open-weight models

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Fast | **hf:zai-org/GLM-4.7-Flash** (default) | $0.10 / $0.50 | 200K ctx / 131K out |
| Best | **hf:moonshotai/Kimi-K2.5** (default) | $0.45 / $3.40 | 200K ctx / 131K out |
|  | hf:deepseek-ai/DeepSeek-V3.2 | $0.56 / $1.68 | 200K ctx / 131K out |

Routing defaults: hf:zai-org/GLM-4.7-Flash (Fast), hf:moonshotai/Kimi-K2.5 (Best)

### TensorX

- **Env var**: `TENSORX_API_KEY`
- **API**: `https://api.tensorx.ai/v1`
- **Features**: Open-weight models, zero data retention, prompt caching

No hardcoded model catalog. Use any model ID supported by this provider.

### Opencode Zen

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Zen API

No hardcoded model catalog. Use any model ID supported by this provider.

By default Caudra hides free models from the Opencode catalog. To list free models (they use a public fallback, no API key needed), add this to `~/.config/caudra/providers.toml`:

```toml
[opencode]
enable_free_models = true
```

The default is `false`.

### xAI

- **Env var**: `XAI_API_KEY` (also supports OAuth via `caudra auth login xai`)
- **API endpoints**:
  - `https://api.x.ai/v1`
  - `https://cli-chat-proxy.grok.com/v1`
- **Features**: OAuth login, account-specific model catalog, Grok reasoning (low/medium/high/xhigh)

| Marker | Models | Pricing (in/out per 1M tokens) | Context |
|---------|--------|-------------------------------|---------|
| Best | **grok-4.6** (default) | $2.00 / $6.00 | 500K ctx / 131K out |
|  | grok-4.5 | $2.00 / $6.00 | 500K ctx / 131K out |
|  | grok-4.3 | $1.25 / $2.50 | 1000K ctx / 131K out |

Routing defaults: grok-4.6 (Best)

OAuth uses the same first-party xAI client as the official Grok CLI (`caudra auth login xai`). Browser login (PKCE) is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically. After login, Caudra fetches your account catalog from `GET /v1/models-v2` on the Grok CLI proxy and caches it for 15 minutes. `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth proxy.

If `~/.grok/auth.json` already exists, login offers to reuse it without writing that file.

### Aperture

- **Env var**: `APERTURE_HOST` (e.g. `https://your-host.tailnet.ts.net`)
- **API**: `Aperture gateway (set APERTURE_HOST)`
- **Features**: Tailscale Aperture LLM gateway; set APERTURE_HOST or configure in providers.toml

Aperture discovers models from your gateway. Set `APERTURE_HOST` to your Tailscale Aperture endpoint (e.g. `https://your-host.tailnet.ts.net`). No API key needed, Tailscale handles auth.

### Opencode Go

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/go/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Go API

No hardcoded model catalog. Use any model ID supported by this provider. An API key is required.


## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
xai/grok-4.6
zai/glm-4.7
```

If the model name is unique across providers, the prefix can be omitted.

## providers.toml

`providers.toml` lives in the config directory (`~/.config/caudra/providers.toml` on Linux/macOS, `%APPDATA%\caudra\providers.toml` on Windows). It is the file for provider overrides and custom HTTP providers. Two jobs:

1. Tweak a built-in (pick a plan, change its base URL, set `enable_free_models` for Opencode).
2. Declare a custom provider that speaks OpenAI, Anthropic, or Google wire format.

```toml
# Point a built-in at a proxy. Env vars still win over this file.
[anthropic]
base_url = "https://my-proxy.internal"

# Full custom provider. Slug becomes the `provider/` prefix in model specs.
[my-proxy]
display_name = "My Proxy"
protocol = "openai"            # openai | openai-responses | anthropic | google
base_url = "https://llm.example.com/v1"
api_key_env = "MY_PROXY_API_KEY"
default_model = "my-proxy/fast-v1"
discover_models = true         # also list models via the provider's /models endpoint

[my-proxy.purposes]
fast = "fast-v1"               # a prefix: also covers fast-v1-turbo, fast-v1-lora, ...
best = ["smart-v1", "smart-v0"]   # a list when one prefix cannot span them

[[my-proxy.models]]
id = "fast-v1"
context_window = 128000
max_output_tokens = 16384
pricing_input = 0.5
pricing_output = 1.5

[[my-proxy.models]]
id = "smart-v1"
context_window = 200000
max_output_tokens = 32000
supports_thinking = true
supports_vision = false
```

The file can start with `version = 1`, and a file without it counts as version 1. Caudra writes the key whenever it saves the file. A newer version stops Caudra with an error rather than being misread. Because `version` belongs to the file, a custom provider cannot use it as a name. See [Config file versions](/docs/configuration/#config-file-versions).

`caudra config example providers` prints every `providers.toml` key with its default, all commented out. [Reference configs](/docs/reference-configs/#providers-toml) shows the same text.

### Provider fields

| Field | Type | Default | Env | Description |
|-------|------|---------|-----|-------------|
| `display_name` | string | the built-in name, or the slug | - | The name pickers and auth status show |
| `protocol` | string | required | - | The wire format: `openai`, `openai-responses`, `anthropic`, or `google` |
| `base_url` | string | the plan URL, or the built-in URL | `<SLUG>_BASE_URL` | The API origin. Caudra appends the protocol paths |
| `plan` | string | unset | - | A built-in plan key, which sets the base URL and the default model |
| `api_key_env` | string | `<SLUG>_API_KEY` | - | The environment variable that holds the API key |
| `api_key` | string | unset | - | An API key, stored as plain text. Caudra tries the environment variable and saved credentials first |
| `default_model` | string | unset | - | The model to use after login when none is saved yet, such as `my-provider/my-model` |
| `discover_models` | bool | `false` | - | Also list the models the provider's model endpoint reports |
| `enable_free_models` | bool | unset | - | Opencode only. Show the free models of its catalog. Unset counts as `false` |
| `overrides` | table | unset | - | Aperture only. Overrides for the upstream providers it routes, keyed by upstream id |
| `model_defaults` | table | unset | - | Model keys for every model of the provider |
| `purposes` | table | unset | - | Model id prefixes for the `fast` and `best` purposes |
| `models` | table[] | unset | - | The models the provider serves |

A built-in slug keeps its compiled protocol, model catalog, and auth setup, so it ignores `protocol`, `api_key_env`, `discover_models`, `models` and `enable_free_models`. Opencode still reads `enable_free_models`.

A `[SLUG.purposes]` table takes these keys:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `fast` | string \| string[] | unset | Model id prefixes for small, fast models, best first. A prefix covers every id that starts with it, and the first one also names the model that fills the slot, so it has to be a real id |
| `best` | string \| string[] | unset | Model id prefixes for flagship models, best first. A prefix cannot also be in `fast`. A model a job is bound to in the picker wins over both lists |

### Model fields

Each `[[SLUG.models]]` entry declares one model:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | required | The model id, which makes the spec `SLUG/ID` |
| `context_window` | integer | discovered, or the protocol default | Tokens of context |
| `max_output_tokens` | integer | discovered, or the protocol default | The most tokens one response may hold |
| `supports_tool_examples` | bool | false | Send tool examples as a structured field. It is off unless declared, because the protocol says nothing about the model behind it |
| `supports_thinking` | bool | discovered, or the protocol default | The model accepts extended thinking |
| `requires_thinking` | bool | false | For an API that rejects requests with thinking off. It implies `supports_thinking` and raises thinking to minimal effort when it is off, compaction included |
| `supports_vision` | bool | false | The model accepts images. When false, image input and `view_image` are off |
| `supports_pdf` | bool | false | `anthropic` and `openai-responses` only. The model reads a PDF that `webfetch` attaches inside its tool result. When it is off, `webfetch` returns the text of the PDF instead |
| `supports_cache_breakpoints` | bool | false | `openai-responses` only. The endpoint honours an explicit `prompt_cache_breakpoint`, so the system prompt closes with one |
| `reasoning_options` | table[] | unset | The reasoning controls the model takes, such as `[{ type = "effort", values = ["low", "high"] }]`. A `type` is `toggle`, `effort` with `values`, or `budget_tokens` with an optional `min` and `max`. `[]` declares that it takes none, so Caudra sends no reasoning level |
| `pricing_input` | float | 0 | USD per million input tokens |
| `pricing_output` | float | 0 | USD per million output tokens |
| `pricing_cache_write` | float | 0 | USD per million tokens written to the prompt cache |
| `pricing_cache_read` | float | 0 | USD per million tokens read from the prompt cache |
| `pricing_fast_input` | float | unset | USD per million input tokens in fast mode |
| `pricing_fast_output` | float | unset | USD per million output tokens in fast mode |

### Model defaults

A `models` entry only applies to the exact `id` it names. When a provider's ids change often, or `discover_models` finds models you never declared, put the shared settings in `model_defaults` instead:

```toml
[my-proxy.model_defaults]
context_window = 229376
max_output_tokens = 32768
reasoning_options = []

[[my-proxy.models]]
id = "smart-v1"
max_output_tokens = 64000
```

It takes the same fields as a `models` entry apart from `id`, and applies to every model of the provider including discovered ones. A matching `models` entry wins field by field, so `smart-v1` above keeps the 229376-token window and raises only its output cap. Anything a model neither declares nor inherits falls back to discovery, then to the protocol default.

`reasoning_options = []` is a declaration, not an omission: it says the endpoint takes no reasoning controls, so Caudra sends no `reasoning_effort`. Leaving it unset instead lets a thinking level chosen for another model reach an endpoint that rejects it.

A `models` entry whose `id` matches no live model is logged once: every setting on it is ignored, which otherwise looks like Caudra disregarding the config.

Custom slugs must not reuse a built-in provider name. A bad TOML parse exits with code 2 at startup so a typo cannot silently empty the registry.

You can also create a custom provider interactively with `caudra auth login` and choosing the custom option. That writes a starter entry to this file.

### Aperture overrides

Aperture proxies upstream providers, exposing each model as `aperture/<upstream>/<model>`. Overrides keyed by upstream provider id live under `[aperture.overrides]`:

```toml
[aperture.overrides.llmserver]
base = "llama-cpp"
context_window = 131072
max_output_tokens = 16384

[aperture.overrides.llmserver.models."qwen-3.6"]
context_window = 262144
supports_vision = true
```

Provider-level fields apply to every model from that upstream. Per-model entries under `models` win field by field, and take the same keys apart from `models`. Model ids containing dots must be quoted (`"qwen3.6"`) since TOML treats a bare dotted key as a nested table.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `context_window` | integer | unset | Tokens of context |
| `max_output_tokens` | integer | unset | The most tokens one response may hold |
| `supports_thinking` | bool | unset | The models accept extended thinking |
| `supports_vision` | bool | unset | The models accept images |
| `base` | string | unset | The native provider an opaque upstream works like, such as `llama-cpp`, `google`, or `anthropic`. Caudra warns about a value it does not know and ignores it |
| `path_prefix` | string | `/v1`, `/v1beta` for Gemini routes, none for Anthropic and Z.AI | The path Caudra sends ahead of each request, which Aperture appends to the upstream base URL. Set it to `""` when that URL already has its own path |
| `models` | table | `{}` | Overrides for single models, keyed by model id, which win key by key. Quote an id that holds a dot, such as `models."qwen-3.6"` |

Caudra sends `/v1` (or `/v1beta` for Gemini routes, nothing for Anthropic and Z.AI), and Aperture appends that path to the upstream's base url. If an upstream base url already carries its own path, set `path_prefix = ""` for it to avoid a doubled path. Z.AI defaults to no prefix since its API path has no `/v1` segment; point the upstream base url at the full API root (e.g. `https://api.z.ai/api/paas/v4`).

### Plans

Some built-ins ship multiple plans (different base URLs or default models). `caudra auth login <provider>` asks which plan to use when more than one exists. You can also set it in TOML:

```toml
[mistral]
plan = "coding"

[zai]
plan = "coding"
```

Current plans:

| Provider | Plan | What it does |
|----------|------|--------------|
| Mistral | `standard` | Standard at `https://api.mistral.ai/v1`, default `mistral/mistral-medium-latest` |
| Mistral | `coding` | Vibe / Coding at `https://api.mistral.ai/v1`, default `mistral/mistral-vibe-cli-latest` |
| Z.AI | `standard` | Pay-as-you-go at `https://api.z.ai/api/paas/v4`, default `zai/glm-5.1` |
| Z.AI | `coding` | Coding plan at `https://api.z.ai/api/coding/paas/v4`, default `zai/glm-5-code` |

Env `<SLUG>_BASE_URL` still wins over both the plan and a `base_url` in this file.

## Dynamic Providers

To add a custom provider or proxy, drop an executable script into the config `providers/` directory (`~/.config/caudra/providers/` on Linux/macOS, `%APPDATA%\caudra\providers\` on Windows). The script must handle these subcommands:

| Subcommand | Timeout | What it does |
|------------|---------|--------|
| `info` | 5s | Return JSON with `display_name`, `base` provider, `has_auth` |
| `models` | 5s | Return JSON array of model entries (optional) |
| `resolve` | 30s | Return auth JSON (`base_url`, `headers`) |
| `login` | interactive | OAuth or credential flow |
| `logout` | interactive | Clear credentials |
| `refresh` | 30s | Refresh auth tokens |

`resolve` is called each time a new agent spawns, so scripts should read tokens from disk instead of caching them in memory. That way auth changes from other processes get picked up.

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: `anthropic`, `openai`, `google`, `copilot`, `ollama`, `llama-cpp`, `mistral`, `zai`, `deepseek`, `openrouter`, `synthetic`, `tensorx`, `opencode`, `xai`, `aperture`.

If your provider serves models not in the base catalog, add a `models` subcommand returning:

```json
[{"id": "my-model-v2", "context_window": 200000, "max_output_tokens": 16384}]
```

Only `id` is required. Optional fields: `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{input, output, cache_write, cache_read}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting), `supports_thinking` (defaults to the base provider's setting), `requires_thinking` (default false; for APIs that reject requests with thinking off, raises it to minimal effort and implies `supports_thinking`), `supports_vision` (defaults to the base provider's setting; when false, image input and the `view_image` tool are disabled). Without this subcommand, the base provider's models are used.

A `llama-cpp` model can replace Caudra's token-budget mapping with its native thinking fields. Each thinking mode maps to a JSON fragment merged into the request body:

```json
[{
  "id": "reasoning-model",
  "supports_thinking": true,
  "thinking_fields": {
    "off": {"reasoning_effort": "none"},
    "adaptive": {"reasoning_effort": "medium"},
    "low": {"reasoning_effort": "low"},
    "medium": {"reasoning_effort": "medium"},
    "xhigh": {"reasoning_effort": "xhigh"}
  }
}]
```

`off` is used when thinking is off, `adaptive` when thinking is on without a chosen level. Any other key is an effort level, one of `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. The levels you declare are the ones the model accepts: whatever you ask for snaps into them, downwards first, so a level the model never advertised is never sent. Every part is optional.

Fragments are merged into the body, so nesting works too. A template toggle is just a fragment:

```json
"thinking_fields": {
  "off": {"chat_template_kwargs": {"enable_thinking": false}},
  "adaptive": {"chat_template_kwargs": {"enable_thinking": true}}
}
```

Named modes send only these fields, no token budget. An explicit `/thinking <budget>` snaps into the levels you declared; a model that declares none gets the `adaptive` fragment plus `thinking_budget_tokens`. Any mode you left undeclared falls back to the usual `thinking_budget_tokens` mapping, so no request ever ends up saying nothing. Models without `thinking_fields` keep the existing llama.cpp behavior.

Dynamic provider models are namespaced as `{slug}/{model_id}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable
