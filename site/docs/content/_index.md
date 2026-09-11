+++
title = "Caudra Docs"
sort_by = "weight"
+++

# Caudra Docs

Caudra turns context into effective action. It is an independent fork and terminal coding agent written in Rust, with models, tools, plugins, and subagents coordinated under your control. The hard-break release line starts at `0.1.0`.

The docs are sorted by what you came here to do:

<div class="doc-group">
  <div class="doc-group-head">
    <span class="eyebrow">Getting Started</span>
    <span class="tagline">start here</span>
  </div>
  <div class="card-grid">
    <a class="card" href="/docs/quick-start/"><span class="card-title">Quick Start</span><span class="card-desc">Install, connect a provider, first session.</span></a>
    <a class="card" href="/docs/configuration/"><span class="card-title">Configuration</span><span class="card-desc">init.lua, the small Lua script where all settings live.</span></a>
  </div>
</div>

<div class="doc-group">
  <div class="doc-group-head">
    <span class="eyebrow">Guides</span>
    <span class="tagline">getting things done</span>
  </div>
  <div class="card-grid">
    <a class="card" href="/docs/skills/"><span class="card-title">Skills</span><span class="card-desc">Write Markdown playbooks the agent loads on demand.</span></a>
    <a class="card" href="/docs/sessions/"><span class="card-title">Sessions</span><span class="card-desc">Fork conversation points and restore chat or workspace state.</span></a>
    <a class="card" href="/docs/review/"><span class="card-title">Review</span><span class="card-desc">Mark passages of a reply and send notes on them back.</span></a>
    <a class="card" href="/docs/workbench/"><span class="card-title">Workbench</span><span class="card-desc">File explorer, editor, source control, and search beside the transcript.</span></a>
    <a class="card" href="/docs/system-prompts/"><span class="card-title">System Prompts</span><span class="card-desc">Create named prompt profiles without copying Caudra's dynamic prompt.</span></a>
    <a class="card" href="/docs/plugins/"><span class="card-title">Plugins</span><span class="card-desc">Add your own tools and commands in Lua, or let the agent write them.</span></a>
    <a class="card" href="/docs/workflows/"><span class="card-title">Workflows</span><span class="card-desc">Durable multi-agent scripts that pause, resume, and report back.</span></a>
    <a class="card" href="/docs/headless/"><span class="card-title">Headless Mode</span><span class="card-desc">--print for scripts and CI. Drop-in Claude Code compatible.</span></a>
    <a class="card" href="/docs/acp/"><span class="card-title">ACP</span><span class="card-desc">Drive Caudra from your editor, like Zed, over the Agent Client Protocol.</span></a>
  </div>
</div>

<div class="doc-group">
  <div class="doc-group-head">
    <span class="eyebrow">Concepts</span>
    <span class="tagline">wondering why</span>
  </div>
  <div class="card-grid">
    <a class="card" href="/docs/token-economy/"><span class="card-title">Token Economy</span><span class="card-desc">Where tokens go in an agent loop, and every trick Caudra uses to spend fewer of them.</span></a>
    <a class="card" href="/docs/context/"><span class="card-title">Context</span><span class="card-desc">What enters the model's context and when, and where to put project knowledge.</span></a>
    <a class="card" href="/docs/changes-from-maki/"><span class="card-title">Changes from Maki</span><span class="card-desc">What changed after the fork point, and why each change exists.</span></a>
    <a class="card" href="/docs/queue/"><span class="card-title">Queue and Steering</span><span class="card-desc">Send work next, guide the current run, or stop and replace it.</span></a>
  </div>
</div>

<div class="doc-group">
  <div class="doc-group-head">
    <span class="eyebrow">Reference</span>
    <span class="tagline">looking something up</span>
  </div>
  <div class="card-grid">
    <a class="card" href="/docs/tools/"><span class="card-title">Tools</span><span class="card-desc">Every built-in tool and its parameters.</span></a>
    <a class="card" href="/docs/providers/"><span class="card-title">Providers</span><span class="card-desc">Model catalogs, env vars, providers.toml, model jobs.</span></a>
    <a class="card" href="/docs/permissions/"><span class="card-title">Permissions</span><span class="card-desc">What runs freely, what asks first, TOML rules.</span></a>
    <a class="card" href="/docs/notifications/"><span class="card-title">Notifications</span><span class="card-desc">Know when a session finishes or needs your input.</span></a>
    <a class="card" href="/docs/mcp/"><span class="card-title">MCP</span><span class="card-desc">External tool servers over stdio or HTTP.</span></a>
    <a class="card" href="/docs/commands/"><span class="card-title">Commands</span><span class="card-desc">The / palette, sessions, toggles, custom commands.</span></a>
    <a class="card" href="/docs/keybindings/"><span class="card-title">Keybindings</span><span class="card-desc">Defaults, precedence, rebinding from Lua.</span></a>
    <a class="card" href="/docs/lua-api/"><span class="card-title">Lua API</span><span class="card-desc">The plugin surface, mirrored from Neovim.</span></a>
    <a class="card" href="/docs/cli/"><span class="card-title">CLI</span><span class="card-desc">Flags and subcommands (auth, models, acp, prompt, ...).</span></a>
    <a class="card" href="/docs/logging/"><span class="card-title">Logging</span><span class="card-desc">The structured log every run writes, and the two ways to read it.</span></a>
    <a class="card" href="/docs/telemetry/"><span class="card-title">Telemetry</span><span class="card-desc">Opt-in OpenTelemetry metrics and events, to a collector you run.</span></a>
  </div>
</div>

Something missing or wrong? Open an issue on [GitHub](https://github.com/caudra/caudra).
