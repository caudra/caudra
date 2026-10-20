use std::fmt::Write;

use caudra_config::example;
use caudra_config::files;
use caudra_config::providers::{BUILTIN_IGNORED_FIELDS, BUILTIN_MODEL_FIELDS};
use caudra_providers::copilot_auth::ENTERPRISE_HOST_ENV;
use caudra_providers::manifest::{ManifestRegistry, ProviderManifest};
use caudra_providers::model::{ModelEntry, ModelPricing};
use caudra_providers::provider::ProviderKind;
use caudra_providers::{EFFORT_LEVELS, ModelMarker};
use strum::IntoEnumIterator;

use crate::gen_regions::{footer, keys_table, reference};
use crate::page_header;

const MODEL_JOBS_NOTE: &str = r#"## Model jobs

Caudra routes work through eleven jobs: **Chat**, **Plan**, **Subagent**, **Compact**, **Title**, **Goal**, **Extract**, **Memory**, **Autopilot**, **Fast**, and **Best**. A global binding can pin a job to an exact `provider/model-id` or make it follow Chat, Plan, Fast, or Best. Explicit bindings report an error when their model is unavailable or disallowed.

`/model` opens a Jobs overview and the model list. Selecting a model on this page changes Chat. Selecting the Chat row jumps to its current model. Select another job to open its assignment page, press `Esc` to return to the overview, and use uppercase `R` to clear the open job's binding. `/goal-model` opens Goal directly. Jobs are opened from the overview rather than cycled with `Tab`.

Press `Ctrl+F` on a model to mark it as a favorite, and again to unmark it. The list shows your favorites first, newest first, then the last 10 models you switched to under Recent. With favorites, `/model` opens on the current model's favorite row, or on the first favorite, instead of on the Chat job. Each model also stays in its provider's section, where a `★` marks a favorite and a `↺` a recent model. `/favorite-next` or `Ctrl+X c` switches Chat to the favorite after the current model, and to the first favorite when the current model is not one. Favorites are saved globally, like bindings, and a favorite that `allowed_models` or `excluded_models` rules out is not shown. A task model name such as `sonnet` runs on the favorite it matches, and the main agent is told your favorites. See [Models and reasoning](/docs/commands/#models-and-reasoning).

The last row of the Jobs section, **Decisions**, names the active decision endpoint, or `off`. Select it to switch endpoints, turn decisions off, or add an endpoint through `/login`. An endpoint whose table does not validate stays listed with its error. `/decision-model` opens that page directly, and `/decision-next` or `Ctrl+X d` switches to the next valid endpoint in name order. Decision endpoints never appear under Favorites or Recent.

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
| Memory | The model currently running the caller |
| Autopilot | Best |
| Fast | Provider `fast` config, curated preferred small model, cheapest priced model, fewest-parameter model, then the anchor |
| Best | Provider `best` config, curated flagship, then the anchor |

The anchor is the selected Chat model when a main turn starts. A Plan binding can select a distinct model, which Caudra uses for main turns sent in Plan mode. An explicit global Subagent binding overrides parent inheritance. A prompt profile's `subagent_model` overrides the global Subagent binding for tasks using that profile. See [System Prompt Profiles](/docs/system-prompts/#configure-subagents). A `model` set on one `task` call overrides both. See [Models and reasoning](/docs/commands/#models-and-reasoning).

See [Sessions](/docs/sessions/#titles) for Title, [Completion goals](/docs/commands/#completion-goals) for Goal, [Requirements](/docs/commands/#requirements) for Extract, [Memory](/docs/memory/#summaries-and-spend) for Memory, and [Autopilot](/docs/autopilot/#the-model-and-its-cost) for Autopilot.

## Supply metadata

Provider catalogs record whether a model is small and whether it is the preferred model in its size lane. The tables below and `caudra models` render those facts with three markers:

| Marker | Meaning |
|--------|---------|
| Small | A small alternative |
| Fast | The preferred small model |
| Best | The provider's flagship |

A known non-small alternative and a model with no supply facts both have no marker. Markers describe provider supply. They are not capability tiers, and price or list order never creates one. Fast may still use price or parameter count as its final provider fallback without adding a marker.

Aggregators such as OpenRouter borrow supply facts from the upstream vendor entry in a vendor-prefixed model id.

Provider `purposes` entries in `providers.toml` define the same supply facts. `fast` entries are small and `best` entries are non-small. The first entry in each list is preferred, so it receives the Fast or Best marker. Later `fast` entries receive Small, while later `best` entries remain unmarked. Entries match by prefix, so an endpoint serving a family of fine-tunes needs one line rather than one per variant:

```toml
[my-server.purposes]
fast = "qwen3.8-27b"   # covers qwen3.8-27b, qwen3.8-27b-canary, qwen3.8-27b-math7, ...
```

The first entry also names the model that wins automatic resolution, so it has to name a model the endpoint actually serves. A prefix that matches nothing live would send requests to an id that does not exist."#;

const AUTH_RELOADING: &str = r#"## Auth Reloading

Caudra re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `caudra auth login` in another terminal or change an env var, the next session picks it up without a restart.

A provider can have several accounts, such as two subscriptions and an API key, and an env var with keys separated by commas (`ANTHROPIC_API_KEY=sk-1,sk-2`) gives one account per key. A login, a logout, or an account choice made in another process reaches a running conversation at its next [cold point](/docs/accounts/#how-auto-switches). See [Accounts](/docs/accounts/) for how Caudra chooses between them."#;

const BASE_URL_OVERRIDES: &str = r#"## Base URL Overrides

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

The built-in provider still owns the slug, so `protocol`, `api_key_env` and `discover_models` are ignored with a warning. Its `model_defaults` and `models` entries only set a model's `context_window` and `max_output_tokens`, see [Model limits on a built-in provider](#model-limits-on-a-built-in-provider). Use a custom slug if you need the rest.

Ollama reads `OLLAMA_HOST` and llama.cpp reads `LLAMA_CPP_HOST`. Neither reads `<SLUG>_BASE_URL`. A `base_url` in `providers.toml` wins over the host variable, and Caudra appends `/v1` to either, so leave it off. Aperture reads `APERTURE_HOST`, which wins over the file. Copilot asks GitHub for the API endpoint of your account, Copilot Enterprise derives it from the host you signed in with, and both ignore these settings."#;

const LONG_CONTEXT_NOTE: &str = r#"Recent Claude models accept up to 1M tokens. Caudra runs them at a 372k working window, which keeps cost and latency bounded. That window is an input budget: the model's output allowance sits on top of it rather than inside it, so Caudra holds back less of it before compaction. Add `-1m` to a model id, like `claude-sonnet-4-6-1m`, to open the full 1M window instead. Set `context_window` in `providers.toml` to pick any other size, see [Model limits on a built-in provider](#model-limits-on-a-built-in-provider). `claude-haiku-5-5` bills a whole request at five times its listed rates once the prompt passes 100k tokens, cached tokens included, and Caudra counts its cost the same way."#;

const GOOGLE_NOTE: &str = r#"Create a `GEMINI_API_KEY` in [Google AI Studio](https://aistudio.google.com/apikey). `gemini-4-argon` needs a key on a project with billing enabled, because the free tier does not serve it. On the free tier, pick `google/gemini-3.8-flash` instead. Where a price above names a date, Caudra bills the first price until that day and the second from 00:00 UTC on it, so an introductory price ends on time without an update. Gemini 4 and Gemini 3 models always think, so thinking `off` runs at the lowest level the model accepts.

A Google AI Pro or Ultra plan covers AI Studio in the browser. Google bills API calls separately, so the plan does not pay for Caudra's requests. Caudra offers no Google sign-in, because Google's terms forbid third-party tools from using the sign-in of Gemini CLI or Antigravity, and Google may suspend accounts that do. On Copilot Pro+, Max, Business or Enterprise, `copilot/gemini-4-argon` runs Argon on your Copilot plan."#;

const ANTHROPIC_OAUTH_NOTE: &str = r#"Run `caudra auth login anthropic` to sign in to a Claude subscription through browser OAuth. Caudra stores the tokens in its state directory, refreshes them automatically, and shows subscription limits through `/usage`. Subscription requests always go to `api.anthropic.com`, even when `ANTHROPIC_BASE_URL` is set.

This experimental flow uses Claude Code's public client registration. Anthropic limits Pro and Max subscription tokens to official clients in its terms. The flow may stop working when Anthropic changes its OAuth or request protocol."#;

const BEDROCK_NOTE: &str = r#"#### Amazon Bedrock

If you already use Claude through AWS Bedrock, you can point Caudra at it instead of the direct Anthropic API. Set `CLAUDE_CODE_USE_BEDROCK=1` and Caudra will route all Anthropic requests through Bedrock. The same models, the same features, just a different door.

You will need `AWS_REGION` and one of the following for auth:

| Method | Env vars |
|--------|----------|
| IAM credentials | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (and optionally `AWS_SESSION_TOKEN`) |
| Credentials file | `AWS_PROFILE` (defaults to `default`), reads `~/.aws/credentials` |
| Bearer token | `AWS_BEARER_TOKEN_BEDROCK` |
| Gateway proxy | `CLAUDE_CODE_SKIP_BEDROCK_AUTH=1` + `ANTHROPIC_BEDROCK_BASE_URL` (skips signing, useful behind a proxy that handles auth) |

You can override the model with `ANTHROPIC_MODEL` and the endpoint with `ANTHROPIC_BEDROCK_BASE_URL`. These env var names match Claude Code, so if you were already using Bedrock there, the same setup works here."#;

const XAI_OAUTH_NOTE: &str = r#"OAuth uses the same first-party xAI client as the official Grok CLI (`caudra auth login xai`). Browser login (PKCE) is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically. After login, Caudra fetches your account catalog from `GET /v1/models-v2` on the Grok CLI proxy and caches it for 15 minutes. `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth proxy.

The Grok CLI proxy only serves clients that declare a recent Grok CLI version, so Caudra sends the version it was tested against. When xAI raises its minimum before you update Caudra, requests fail with HTTP 426 and the error names the version the proxy wants. Set `CAUDRA_XAI_CLIENT_VERSION` to that version to keep working until you update.

If `~/.grok/auth.json` already exists, login offers to reuse it without writing that file."#;

const COPILOT_OAUTH_NOTE: &str = r#"Run `caudra auth login copilot` to sign in with GitHub. Caudra prints a URL and a one-time code, opens the browser when it can, and waits until you approve the code on GitHub. The login uses the same public GitHub OAuth app as OpenCode and asks only for the `read:user` scope. GitHub issues a token that does not expire, so there is no refresh. When GitHub revokes it, run the login again.

To reuse a token from the gh CLI, the Copilot editor plugin, or the system keyring instead, add `--method import`. Caudra stores the token in its state directory and labels it `token` in `caudra auth status`. `GH_COPILOT_TOKEN` or `COPILOT_GITHUB_TOKEN` wins over the saved token.

Copilot lists a prompt limit for each model beside its total window, and refuses a longer prompt even when the total has room. Caudra uses that prompt limit as the model's window, with the output allowance on top of it, so a session compacts before Copilot refuses it."#;

const COPILOT_ENTERPRISE_NOTE: &str = r#"Copilot Enterprise is a separate provider, so a GitHub Enterprise account and a personal account can stay signed in together. Run `caudra auth login copilot-enterprise --hostname company.ghe.com`, or leave out `--hostname` and Caudra asks for it. The login runs against that host and saves the token together with the host. `--method import` reads gh CLI and Copilot plugin tokens for that host only.

Caudra sends requests to `https://copilot-api.<host>` and never falls back to public GitHub. This follows the endpoint scheme OpenCode uses for GitHub Enterprise Cloud with data residency. Other deployments may not serve Copilot there. For CI, set `GH_COPILOT_ENTERPRISE_TOKEN` and `GH_COPILOT_ENTERPRISE_HOST` together. The token variable alone is an error, never a public GitHub token.

The models are the ones Copilot lists above, under `copilot-enterprise/`, such as `copilot-enterprise/gpt-5.6-terra`. Your organization's policy decides which of them your account can use."#;

const OPENCODE_FREE_MODELS_NOTE: &str = r#"By default Caudra hides free models from the Opencode catalog. To list free models (they use a public fallback, no API key needed), add this to `~/.config/caudra/providers.toml`:

```toml
[opencode]
enable_free_models = true
```

The default is `false`."#;

const OPENCODE_GO_SECTION: &str = r#"### Opencode Go

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/go/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Go API

No hardcoded model catalog. Use any model ID supported by this provider. An API key is required.
"#;

const MODEL_IDENTIFIERS: &str = r#"## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
xai/grok-4.6
zai/glm-4.7
```

If the model name is unique across providers, the prefix can be omitted."#;

fn providers_toml_section() -> String {
    let mut plan_rows = String::new();
    let mut plan_examples = String::new();
    let mut builtins: Vec<_> = caudra_config::providers::all_builtins();
    builtins.sort_by_key(|b| b.slug);
    let mut wrote_example = false;
    for b in builtins {
        let Some(plans) = b.plans.filter(|p| p.len() > 1) else {
            continue;
        };
        if !wrote_example {
            let _ = writeln!(plan_examples, "```toml");
            wrote_example = true;
        } else {
            let _ = writeln!(plan_examples);
        }
        // Prefer a non-default plan key in the example when one exists.
        let example_key = plans
            .iter()
            .find(|(_, p)| {
                p.base_url != b.default_base_url || p.default_model != Some(b.default_model)
            })
            .unwrap_or(&plans[0])
            .0;
        let _ = writeln!(plan_examples, "[{}]", b.slug);
        let _ = writeln!(plan_examples, "plan = \"{example_key}\"");
        for (key, plan) in plans {
            let mut detail = plan.display_name.to_string();
            if !plan.base_url.is_empty() {
                detail = format!("{detail} at `{}`", plan.base_url);
            }
            if let Some(model) = plan.default_model {
                detail = format!("{detail}, default `{model}`");
            }
            let _ = writeln!(plan_rows, "| {} | `{key}` | {detail} |", b.display_name);
        }
    }
    if wrote_example {
        let _ = writeln!(plan_examples, "```");
    }

    let plans_body = if plan_rows.is_empty() {
        "No built-in currently ships more than one plan.".to_string()
    } else {
        format!(
            "Some built-ins ship multiple plans (different base URLs or default models). \
`caudra auth login <provider>` asks which plan to use when more than one exists. \
You can also set it in TOML:\n\n\
{plan_examples}\n\
Current plans:\n\n\
| Provider | Plan | What it does |\n\
|----------|------|--------------|\n\
{plan_rows}\n\
Env `<SLUG>_BASE_URL` still wins over both the plan and a `base_url` in this file."
        )
    };

    let document = reference(&files::PROVIDERS);
    let provider_fields = keys_table(&document, &[example::providers::PROVIDER]);
    let purpose_fields = keys_table(&document, &[example::providers::PURPOSES]);
    let model_fields = keys_table(&document, &[example::providers::MODELS]);
    let override_fields = keys_table(&document, &[example::providers::UPSTREAM]);
    let switching_fields = keys_table(&document, &[example::providers::SWITCHING]);
    let account_fields = keys_table(&document, &[example::providers::ACCOUNTS]);
    let decision_fields = keys_table(&document, &[example::providers::DECISION_ENDPOINT]);
    let provider_order_fields = keys_table(&document, &[example::providers::PROVIDER_ORDER]);
    let builtin_ignored = join_and(&BUILTIN_IGNORED_FIELDS.map(|name| format!("`{name}`")));
    let builtin_model = join_and(&BUILTIN_MODEL_FIELDS.map(|name| format!("`{name}`")));
    let reference = footer(&files::PROVIDERS);

    format!(
        r#"## providers.toml

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

{reference}

### Provider fields

{provider_fields}
A built-in slug keeps its compiled protocol, model catalog, and auth setup, so it ignores {builtin_ignored}. Opencode still reads `enable_free_models`. From `model_defaults` and `models`, a built-in reads only {builtin_model}, see [Model limits on a built-in provider](#model-limits-on-a-built-in-provider).

A `[SLUG.purposes]` table takes these keys:

{purpose_fields}
### Model fields

Each `[[SLUG.models]]` entry declares one model:

{model_fields}
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

On an `openai` or `openai-responses` provider, a model gets a thinking level only from its declared `reasoning_options`. A model that declares no levels takes no thinking setting: Caudra sends no `reasoning_effort`, and the footer and task results show no level for it, because the endpoint would reject one it does not know. Declare the levels to offer them. On `anthropic` and `google`, `reasoning_options = []` is a declaration, not an omission: it says the endpoint takes no reasoning controls. Leaving it unset there lets a thinking level chosen for another model reach an endpoint that may reject it.

A `models` entry whose `id` matches no live model is logged once: every setting on it is ignored, which otherwise looks like Caudra disregarding the config.

Custom slugs must not reuse a built-in provider name. A bad TOML parse exits with code 2 at startup so a typo cannot silently empty the registry. A bad edit while Caudra runs keeps the last valid file and shows the error, see [Hot reload](/docs/configuration/#hot-reload).

You can also create a custom provider interactively with `caudra auth login` and choosing the custom option. That writes a starter entry to this file.

### Model limits on a built-in provider

A built-in provider reads two model keys from this file: `context_window` and `max_output_tokens`. Set them in a `[[SLUG.models]]` entry for one model, or in `[SLUG.model_defaults]` for every model of the provider:

```toml
[[anthropic.models]]
id = "claude-sonnet-4-6"
context_window = 200000
```

A `models` entry wins over `model_defaults`, and both win over the limits the provider lists, the built-in model table, and models.dev, in that order. A declared window is the total the prompt and the reply share, as on a custom provider, so compaction leaves room in it for the reply. The OpenAI Coding Plan still caps a declared window at the plan's own. On `aperture`, a declared window also wins over [Aperture overrides](#aperture-overrides).

An entry adds no model to the model list, and its `id` has to match the id in the model spec exactly. Every other model key on a built-in slug, such as `supports_vision` or a price, is ignored with a warning. Caudra reads an edit the next time it resolves the model, such as in a new session or after you pick the model again, see [Hot reload](/docs/configuration/#hot-reload).

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

{override_fields}
Caudra sends `/v1` (or `/v1beta` for Gemini routes, nothing for Anthropic and Z.AI), and Aperture appends that path to the upstream's base url. If an upstream base url already carries its own path, set `path_prefix = ""` for it to avoid a doubled path. Z.AI defaults to no prefix since its API path has no `/v1` segment; point the upstream base url at the full API root (e.g. `https://api.z.ai/api/paas/v4`).

### Accounts

`[[SLUG.accounts]]` entries list a provider's [accounts](/docs/accounts/) in priority order, and `[SLUG.switching]` sets how Auto moves conversations between them. A table that holds only these two changes nothing else about a built-in provider:

```toml
[anthropic.switching]
strategy = "paced"
prefer_below = 25

[[anthropic.accounts]]
name = "personal"

[[anthropic.accounts]]
name = "api"
fallback = true
```

A `[SLUG.switching]` table takes these keys:

{switching_fields}
Each `[[SLUG.accounts]]` entry takes these keys:

{account_fields}
See [How Auto switches](/docs/accounts/#how-auto-switches) for what the thresholds do.

A top-level `[switching]` table lets Auto move a conversation to another provider's subscription once its own provider's subscriptions run low. It is off until the table lists at least two providers:

```toml
[switching]
providers = ["anthropic", "openai"]
```

{provider_order_fields}
See [Across providers](/docs/accounts/#across-providers) for which model the conversation moves to and when it comes back.

### Decision endpoints

The optional typed decision engine behind [Auto mode](/docs/permissions/#auto-mode), workflow [`decide()` calls](/docs/workflows/#typed-decisions), and the [`decide()` and `rerank()` helpers](/docs/code/#decisions-and-reranking) sends its requests to a decision endpoint. Each endpoint is a `[decisions.<name>]` table in this file. One endpoint is active at a time, and with none active the engine is off. The settings every endpoint shares, such as deadlines, features, and thresholds, stay in [`[decisions]`](/docs/configuration/#decisions) in `caudra.toml`.

The quickest way to add one is `caudra auth login --decision`, or the **Decision models** section of `/login`. Both offer two presets, **TypeSafe Jev** (`typesafe`) and **OpenAI Luna** (`openai`), and a custom endpoint, which asks for a name, the protocol, the base URL, the model, and a key. Login writes the table, saves the key, and makes a new endpoint active. Logging in again to an existing endpoint replaces its key and keeps whichever endpoint is active. The presets write these tables:

```toml
[decisions.typesafe]
protocol = "typesafe"
base_url = "https://api.typesafe.ai"
model = "jev-latest"
api_key_env = "TYPESAFE_API_KEY"

[decisions.openai]
protocol = "openai"
base_url = "https://api.openai.com/v1"
model = "gpt-6-luna"
api_key_env = "OPENAI_API_KEY"
```

{decision_fields}
`protocol = "typesafe"` appends `/v1/systemone` to a root base URL. A path prefix stays in place, so a server mounted under `/typesafe` uses `base_url = "http://127.0.0.1:8080/typesafe"` and receives requests at `/typesafe/v1/systemone`. `protocol = "openai"` appends `/decisions` to a versioned base URL, such as `https://api.openai.com/v1` for the hosted OpenAI Decisions API. Leave the appended path out of `base_url`, because a full endpoint is rejected.

Configuring an endpoint is the consent to send decision context to it, so HTTPS needs no other opt-in. Plain HTTP to numeric loopback, such as `127.0.0.1` or `[::1]`, needs nothing either. Plain HTTP to any other host needs `allow_http = true`, which sends decision context and the key without TLS. Use it only with transport protection you control, such as a trusted encrypted tunnel. `localhost` is a DNS name for this rule, and private and CGNAT addresses get no exemption. For a local ninfer server, or one behind a tunnel, use the OpenAI protocol with the model name the server actually serves. `systemone-decision-v7` is not a Luna alias:

```toml
[decisions.local]
protocol = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "systemone-decision-v7"

[decisions.tunnel]
protocol = "openai"
base_url = "http://100.64.0.2:8080/v1"
model = "systemone-decision-v7"
allow_http = true
```

Requests to an endpoint that is not a numeric loopback address go through the [network proxy](/docs/cli/#network-proxies). Requests do not follow redirects. These settings do not change Workcell transport policy.

A key saved by login, in `auth/decisions/<name>.json` under the state directory, wins. Otherwise Caudra reads the global environment variable that `api_key_env` names, never a value from a project `.env` file. A custom endpoint has no `api_key_env` until you add one, and an endpoint with no key sends none, which suits a local server. To take the base URL from the environment, write `base_url = "{{env:TYPESAFE_BASE_URL}}"`. The `TYPESAFE_BASE_URL` and `OPENAI_BASE_URL` variables do not change a decision endpoint on their own.

Switch the active endpoint with the **Decisions** row of `/model`, `/decision-model`, `/decision-next` (`Ctrl+X d`), or `caudra decisions use <name|off>`. `caudra decisions list` shows every endpoint, its key source, and which one is active. The choice is saved globally. A switch in the TUI applies to every open session of that process at once, and other Caudra processes and headless runs read it when they start. An edit to the active endpoint's table applies when you select the endpoint again or restart. When the active endpoint's table is gone, decisions are off and Caudra logs a warning that names it. `caudra auth logout --decision <name>` deletes the saved key and the table, and turns decisions off when that endpoint was active.

### Plans

{plans_body}"#
    )
}

fn dynamic_providers_section() -> String {
    let valid_values: Vec<String> = ProviderKind::iter().map(|k| format!("`{k}`")).collect();
    let efforts: Vec<String> = EFFORT_LEVELS.iter().map(|e| format!("`{e}`")).collect();

    format!(
        r#"## Dynamic Providers

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

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: {}.

If your provider serves models not in the base catalog, add a `models` subcommand returning:

```json
[{{"id": "my-model-v2", "context_window": 200000, "max_output_tokens": 16384}}]
```

Only `id` is required. Optional fields: `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{{input, output, cache_write, cache_read}}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting), `supports_thinking` (defaults to the base provider's setting), `requires_thinking` (default false; for APIs that reject requests with thinking off, raises it to minimal effort and implies `supports_thinking`), `supports_vision` (defaults to the base provider's setting; when false, image input and the `view_image` tool are disabled). Without this subcommand, the base provider's models are used.

A `llama-cpp` model can replace Caudra's token-budget mapping with its native thinking fields. Each thinking mode maps to a JSON fragment merged into the request body:

```json
[{{
  "id": "reasoning-model",
  "supports_thinking": true,
  "thinking_fields": {{
    "off": {{"reasoning_effort": "none"}},
    "adaptive": {{"reasoning_effort": "medium"}},
    "low": {{"reasoning_effort": "low"}},
    "medium": {{"reasoning_effort": "medium"}},
    "xhigh": {{"reasoning_effort": "xhigh"}}
  }}
}}]
```

`off` is used when thinking is off, `adaptive` when thinking is on without a chosen level. Any other key is an effort level, one of {}. The levels you declare are the ones the model accepts: whatever you ask for snaps into them, downwards first, so a level the model never advertised is never sent. Every part is optional.

Fragments are merged into the body, so nesting works too. A template toggle is just a fragment:

```json
"thinking_fields": {{
  "off": {{"chat_template_kwargs": {{"enable_thinking": false}}}},
  "adaptive": {{"chat_template_kwargs": {{"enable_thinking": true}}}}
}}
```

Named modes send only these fields, no token budget. An explicit `/thinking <budget>` snaps into the levels you declared; a model that declares none gets the `adaptive` fragment plus `thinking_budget_tokens`. Any mode you left undeclared falls back to the usual `thinking_budget_tokens` mapping, so no request ever ends up saying nothing. Models without `thinking_fields` keep the existing llama.cpp behavior.

Dynamic provider models are namespaced as `{{slug}}/{{model_id}}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable"#,
        valid_values.join(", "),
        efforts.join(", "),
    )
}

/// Today's rates, then any announced change with its date. Reads no clock, so
/// the page renders the same whenever it is generated.
fn format_pricing(entry: &ModelEntry, manifest: &ProviderManifest) -> String {
    let rates = |pricing: &ModelPricing| format!("${:.2} / ${:.2}", pricing.input, pricing.output);
    match manifest.price_change(entry) {
        Some(change) => format!(
            "{}, then {} from {}",
            rates(&entry.pricing),
            rates(&change.pricing),
            change.from
        ),
        None => rates(&entry.pricing),
    }
}

fn format_context(entry: &ModelEntry) -> String {
    let ctx_k = entry.context_window / 1_000;
    match entry.max_output_tokens {
        Some(out) => format!("{ctx_k}K ctx / {}K out", out / 1_000),
        None => format!("{ctx_k}K ctx"),
    }
}

struct ProviderSection {
    kind: ProviderKind,
    name: &'static str,
    auth_line: String,
    urls: Vec<&'static str>,
    features: Option<&'static str>,
    manifest: &'static ProviderManifest,
}

fn format_auth(kind: ProviderKind) -> String {
    let env = kind.api_key_env();
    if kind == ProviderKind::Ollama {
        format!("`OLLAMA_HOST` for local/remote (e.g. `http://localhost:11434`), `{env}` for auth")
    } else if kind == ProviderKind::Aperture {
        "`APERTURE_HOST` (e.g. `https://your-host.tailnet.ts.net`)".into()
    } else {
        format!("`{env}`")
    }
}

fn build_sections() -> Vec<ProviderSection> {
    let mut sections = Vec::new();

    for kind in ProviderKind::iter() {
        match kind {
            ProviderKind::Zai => {
                sections.push(ProviderSection {
                    kind: ProviderKind::Zai,
                    name: "Z.AI",
                    auth_line: format!(
                        "{} (shared across both endpoints)",
                        format_auth(ProviderKind::Zai)
                    ),
                    urls: vec![
                        ProviderKind::Zai.base_url(),
                        "https://api.z.ai/api/coding/paas/v4",
                    ],
                    features: ProviderKind::Zai.features(),
                    manifest: ManifestRegistry::get("zai").unwrap(),
                });
            }
            ProviderKind::OpenAi => {
                sections.push(ProviderSection {
                    kind,
                    name: kind.display_name(),
                    auth_line: format!("{} (also supports OAuth device flow)", format_auth(kind)),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    manifest: ManifestRegistry::get(&kind.to_string()).unwrap(),
                });
            }
            ProviderKind::Anthropic => {
                sections.push(ProviderSection {
                    kind,
                    name: kind.display_name(),
                    auth_line: format!(
                        "{} (also supports subscription OAuth via `caudra auth login anthropic`)",
                        format_auth(kind)
                    ),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    manifest: ManifestRegistry::get(&kind.to_string()).unwrap(),
                });
            }
            ProviderKind::Xai => {
                sections.push(ProviderSection {
                    kind,
                    name: kind.display_name(),
                    auth_line: format!(
                        "{} (also supports OAuth via `caudra auth login xai`)",
                        format_auth(kind)
                    ),
                    urls: vec![kind.base_url(), "https://cli-chat-proxy.grok.com/v1"],
                    features: kind.features(),
                    manifest: ManifestRegistry::get(&kind.to_string()).unwrap(),
                });
            }
            ProviderKind::Copilot => {
                sections.push(ProviderSection {
                    kind,
                    name: kind.display_name(),
                    auth_line: format!(
                        "{} or `COPILOT_GITHUB_TOKEN` (or run `caudra auth login copilot` to sign in with GitHub)",
                        format_auth(kind)
                    ),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    manifest: ManifestRegistry::get(&kind.to_string()).unwrap(),
                });
            }
            ProviderKind::CopilotEnterprise => {
                sections.push(ProviderSection {
                    kind,
                    name: kind.display_name(),
                    auth_line: format!(
                        "{} with `{ENTERPRISE_HOST_ENV}` (or run `caudra auth login copilot-enterprise` to sign in with GitHub)",
                        format_auth(kind)
                    ),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    manifest: ManifestRegistry::get(&kind.to_string()).unwrap(),
                });
            }
            _ => {
                sections.push(ProviderSection {
                    kind,
                    name: kind.display_name(),
                    auth_line: format_auth(kind),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    manifest: ManifestRegistry::get(&kind.to_string()).unwrap(),
                });
            }
        }
    }

    sections
}

fn write_model_table(out: &mut String, manifest: &ProviderManifest) {
    let entries = manifest.models;
    let line_of = |entry: &ModelEntry| {
        manifest
            .generations
            .iter()
            .find(|line| line.contains(entry))
            .map(|line| line.label)
    };
    let by_lane = || {
        [true, false]
            .into_iter()
            .flat_map(move |small| entries.iter().filter(move |entry| entry.small == small))
    };
    let label = |entry: &ModelEntry| {
        let prefix = entry.prefixes.first()?;
        let marker = entry.facts().marker()?;
        Some(format!("{prefix} ({})", marker.label()))
    };

    let _ = writeln!(
        out,
        "| Marker | Models | Pricing (in/out per 1M tokens) | Context |"
    );
    let _ = writeln!(
        out,
        "|---------|--------|-------------------------------|---------|"
    );

    for entry in by_lane() {
        let names = entry.prefixes.join(", ");
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            entry
                .facts()
                .marker()
                .map(ModelMarker::label)
                .unwrap_or_default(),
            match (entry.default, line_of(entry)) {
                (false, _) => names,
                (true, None) => format!("**{names}** (default)"),
                (true, Some(line)) => format!("**{names}** ({line} default)"),
            },
            format_pricing(entry, manifest),
            format_context(entry),
        );
    }

    // The first default per lane in table order, which is the answer
    // `find_default_for_purpose` gives a session with no model to read a line
    // from. Tables are ordered newest line first, so that is the newest line.
    let wide: Vec<String> = [true, false]
        .into_iter()
        .filter_map(|small| {
            entries
                .iter()
                .find(|entry| entry.small == small && entry.default)
        })
        .filter_map(label)
        .collect();

    if !wide.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Routing defaults: {}", wide.join(", "));
    }

    let lines: Vec<String> = manifest
        .generations
        .iter()
        .filter_map(|line| {
            let within: Vec<String> = by_lane()
                .filter(|entry| entry.default && line_of(entry) == Some(line.label))
                .filter_map(label)
                .collect();
            (!within.is_empty()).then(|| format!("On {} that is {}", line.label, join_and(&within)))
        })
        .collect();

    if !lines.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "A lane answers inside the release line you are on. {}.",
            lines.join(". ")
        );
    }
}

/// Oxford-free list for prose, where a trailing comma before "and" would read
/// as another item.
pub fn join_and(items: &[String]) -> String {
    match items.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

fn no_catalog_note(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Ollama => {
            "This provider talks the OpenAI-compatible `/v1` API, so it also works with \
             llama.cpp's server, LocalAI, or anything else that speaks the same protocol. \
             Just point `OLLAMA_HOST` to the right address \
             (e.g. `http://localhost:8080` for llama.cpp)."
        }
        ProviderKind::LlamaCpp => {
            "Connects to any OpenAI-compatible `/v1` endpoint. Set `LLAMA_CPP_HOST` to your server \
             address, such as `http://localhost:8080`, or run `caudra auth login llama-cpp`, which \
             offers that address and saves your answer to `providers.toml`. Without either, Caudra \
             reports that `LLAMA_CPP_HOST` is not set."
        }
        ProviderKind::Aperture => {
            "Aperture discovers models from your gateway. Set `APERTURE_HOST` to your Tailscale Aperture \
             endpoint (e.g. `https://your-host.tailnet.ts.net`). No API key needed, Tailscale handles auth."
        }
        ProviderKind::OpenRouter => {
            "OpenRouter aggregates models from many providers behind a single API key. \
             Browse available models at [openrouter.ai/models](https://openrouter.ai/models). \
             Use any model ID directly (e.g. `openrouter/anthropic/claude-sonnet-4`)."
        }
        _ => "No hardcoded model catalog. Use any model ID supported by this provider.",
    }
}

fn write_section(out: &mut String, section: &ProviderSection) {
    let _ = writeln!(out, "### {}\n", section.name);
    let _ = writeln!(out, "- **Env var**: {}", section.auth_line);

    if section.urls.len() == 1 {
        let _ = writeln!(out, "- **API**: `{}`", section.urls[0]);
    } else {
        let _ = writeln!(out, "- **API endpoints**:");
        for url in &section.urls {
            let _ = writeln!(out, "  - `{url}`");
        }
    }

    if let Some(features) = section.features {
        let _ = writeln!(out, "- **Features**: {features}");
    }

    // Rendered from the schedule, so the docs cannot drift from what we bill.
    if let Some(schedule) = ManifestRegistry::get(&section.kind.to_string())
        .and_then(|manifest| manifest.pricing_schedule)
    {
        let _ = writeln!(
            out,
            "- **Peak pricing**: the prices below are off-peak; each turn is billed as it happens, at {schedule}"
        );
    }

    let _ = writeln!(out);

    if section.kind == ProviderKind::CopilotEnterprise {
        let _ = writeln!(out, "{COPILOT_ENTERPRISE_NOTE}");
        return;
    }

    if section.manifest.models.is_empty() {
        let _ = writeln!(out, "{}", no_catalog_note(section.kind));
    } else {
        write_model_table(out, section.manifest);
    }

    if section.kind == ProviderKind::Copilot {
        let _ = writeln!(out, "\n{COPILOT_OAUTH_NOTE}");
    }

    if section.kind == ProviderKind::Google {
        let _ = writeln!(out, "\n{GOOGLE_NOTE}");
    }

    if section.name == "Anthropic" {
        let _ = writeln!(out, "\n{ANTHROPIC_OAUTH_NOTE}");
        let _ = writeln!(out, "\n{LONG_CONTEXT_NOTE}");
        let _ = writeln!(out, "\n{BEDROCK_NOTE}");
    }

    if section.kind == ProviderKind::Opencode {
        let _ = writeln!(out, "\n{OPENCODE_FREE_MODELS_NOTE}");
    }

    if section.kind == ProviderKind::Xai {
        let _ = writeln!(out, "\n{XAI_OAUTH_NOTE}");
    }
}

pub fn generate() -> String {
    let mut out = page_header(
        "Providers",
        "Model catalogs, env vars, providers.toml, model jobs.",
    );
    let _ = writeln!(
        out,
        "Caudra talks to LLM providers over their HTTP APIs. Model jobs decide \
         which configured or discovered model serves each kind of work.\n"
    );
    let _ = writeln!(out, "{MODEL_JOBS_NOTE}\n");
    let _ = writeln!(out, "{AUTH_RELOADING}\n");
    let _ = writeln!(out, "{BASE_URL_OVERRIDES}\n");
    let _ = writeln!(out, "## Built-in Providers\n");

    for section in &build_sections() {
        write_section(&mut out, section);
        let _ = writeln!(out);
    }

    // Opencode Go is catalog-backed (no ProviderKind), so it gets a static
    // section right after Opencode Zen, which is the last built-in section.
    let _ = writeln!(out, "{OPENCODE_GO_SECTION}\n");

    let _ = writeln!(out, "{MODEL_IDENTIFIERS}\n");
    let _ = writeln!(out, "{}\n", providers_toml_section());
    let _ = writeln!(out, "{}", dynamic_providers_section());

    out
}

#[cfg(test)]
mod tests {
    use caudra_config::files;
    use caudra_config::interpolation::{self, Lookup};
    use caudra_config::migration;
    use caudra_config::providers::{PROVIDERS_FORMAT, ProvidersConfig};
    use caudra_providers::manifest::ManifestRegistry;
    use caudra_providers::model::ModelEntry;
    use test_case::test_case;

    use super::{format_pricing, providers_toml_section};
    use crate::gen_regions::reference;

    const TOML_FENCE: &str = "```toml\n";
    const FENCE_END: &str = "```";
    const DECISION_HEADER: &str = "[decisions.";

    #[test_case("gemini-4-argon", "$2.00 / $10.00, then $4.00 / $20.00 from 2026-12-01" ; "an_announced_change_follows_todays_price")]
    #[test_case("gemini-3.5-flash-lite", "$0.30 / $2.50"                               ; "a_steady_price_stands_alone")]
    fn pricing_shows_an_announced_change_with_its_date(prefix: &str, expected: &str) {
        let google = ManifestRegistry::get("google").unwrap();
        let entry: &ModelEntry = google
            .models
            .iter()
            .find(|entry| entry.prefixes.contains(&prefix))
            .unwrap();
        assert_eq!(format_pricing(entry, google), expected);
    }

    #[test]
    fn the_providers_section_lists_every_key_of_the_reference() {
        let section = providers_toml_section();
        for table in reference(&files::PROVIDERS).tables {
            for entry in table.entries {
                let row = format!("| `{}` |", entry.name);
                assert!(section.contains(&row), "{row}");
            }
        }
    }

    #[test]
    fn every_decision_endpoint_example_resolves() {
        let section = providers_toml_section();
        let examples: Vec<ProvidersConfig> = section
            .split(TOML_FENCE)
            .skip(1)
            .filter_map(|rest| rest.split_once(FENCE_END).map(|(body, _)| body))
            .filter(|body| body.contains(DECISION_HEADER))
            .map(|body| {
                let loaded = migration::upgrade_text(body.to_owned(), &PROVIDERS_FORMAT).unwrap();
                interpolation::from_str(&loaded.text, Lookup::Refused).unwrap()
            })
            .collect();
        assert!(!examples.is_empty());
        for (name, def) in examples.iter().flat_map(|config| &config.decisions) {
            def.resolve(name).unwrap();
        }
    }
}
