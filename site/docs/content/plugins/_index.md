+++
title = "Plugins"
weight = 23
[extra]
group = "Guides"
+++

# Writing maki plugins

Maki plugins are plain Lua files (Luau) that run inside maki. A plugin can
register tools the LLM calls, slash commands, keymaps, prompt hints, and
custom UI. Everything lives under the global `maki` table. The full API
reference is at the end of this document.

## Where plugin code goes

Plugins live in one of two config directories with the same layout:

- `maki.env.config_dir()` - global, every project
- `<project>/.maki/` - this project only

Release builds normally return `~/.config/maki/`. Debug builds return
`~/.config/maki-debug/`. An active legacy directory takes precedence.

```
init.lua        the only file maki runs; require()s plugins, calls maki.setup()
lua/<name>.lua  plugin modules, loaded by require("<name>")
plugin.toml     permission grants for every Lua file in the dir
```

Nothing under `lua/` loads on its own. A module name is its path under `lua/`
without the extension: `lua/browser.lua` is `require("browser")`,
`lua/acme/tools.lua` is `require("acme.tools")`. `require` is sandboxed to
that directory, you cannot reach files outside it.

## Creating a plugin

1. Write the code in `<config>/lua/<name>.lua`, where `<config>` is the global
   config directory above. The `maki` global is
   already there, nothing to import. For a project-only plugin use
   `<project>/.maki/` here and in every step below.

```lua
maki.api.register_tool({
  name = "hello",
  description = "Say hello to a name.",
  parameters = { type = "object", properties = { name = { type = "string" } }, required = { "name" } },
  handler = function(args)
    return { llm_output = "hello " .. args.name }
  end,
})
```

2. Load it from `<config>/init.lua`, creating that file if missing:

```lua
require("hello")
```

3. Grant the permissions it needs in `<config>/plugin.toml`, creating
   that file if missing. Without the file every gated call is denied.

```toml
[permissions]
fs_read = true
run = true
```

4. Run `/reload`, then read the log as described below, to see that it loaded
   and what it printed.

Leave `maki.api.register_options` to bundled plugins: maki rejects a
`plugins.<name>` table for a plugin it does not ship, and startup fails. Keep
settings in a local table, or export a `setup(opts)` function `init.lua` calls.

## Permissions and plugin.toml

Sensitive APIs are gated per plugin file, and a plugin without a
`plugin.toml` next to it gets nothing. The gates and the file format are
in [the reference](/docs/lua-api/#plugin-permissions).

## Development loop

`/reload` rebuilds plugins and config in place, no restart needed. Until it
runs, an edited plugin is still the old one.

To debug, add `maki.log.info|warn|error(...)` calls. They write to `maki.log`
in the directory `maki.env.logs_dir()` returns. When
a backtrace comes out useless, start maki with `--no-jit`: plugins then run on
the interpreter, with full debug info.

## Conventions

- Fallible runtime calls return a `(value, err)` pair; check `err` before using `value`.
- Tool handlers report failures with `{ llm_output = "error: ...", is_error = true }`, not by raising.
- Return complete `llm_output` from tool handlers. The host applies output limits and retains eligible full text for later retrieval.
- Set `output_limits = { max_lines = ..., max_bytes = ... }` only to override the host limits for one result. Use `maki.truncate` only when producer-level loss is intentional.
- The model picks tools by reading `description`, so state precisely what the tool does and when to use it.
- Reusable helpers ship with maki; see "Shared helper modules" in the API reference.

## A complete real example

The bundled `view_image` tool, verbatim: schema, header and restore hooks, error
handling, host-managed output limits, collapsible UI view. It is a bundled plugin,
so it opens with `register_options`, which your own plugin skips:

```lua
local shorten_path = require("maki.shorten_path")

local DESCRIPTION =
  [[View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `file_read` for images.

- Paths: absolute, relative, or ~/.
- Oversized images are downscaled automatically (animated gif/webp keep only the first frame).]]

-- Anthropic rejects images over 5MB base64; 3MB raw is ~4MB encoded,
-- which leaves headroom.
local MAX_RAW_BYTES = 3 * 1024 * 1024
-- Anthropic downscales anything over 1568px on the long edge server-side
-- anyway, so ship fewer bytes and do it here.
local MAX_EDGE = 1568
-- Refuse absurdly large files up front; maki.image.decode also enforces a
-- host-side pixel cap against decode bombs.
local MAX_INPUT_BYTES = 50 * 1024 * 1024

local MEDIA_TYPES = {
  png = "image/png",
  jpeg = "image/jpeg",
  gif = "image/gif",
  webp = "image/webp",
}

local function format_size(bytes)
  if bytes >= 1024 * 1024 then
    return string.format("%.1fMB", bytes / (1024 * 1024))
  end
  return string.format("%dKB", math.ceil(bytes / 1024))
end

local function caption(path, bytes, width, height, note)
  -- Shortened path, not basename: two screenshot.png in different dirs must
  -- stay distinguishable when several images land in one turn.
  return string.format("[image: %s %s %dx%d%s]", shorten_path(path), format_size(bytes), width, height, note or "")
end

local function fail(msg)
  return { llm_output = msg, is_error = true }
end

local function load_image(path)
  local bytes, read_err = maki.fs.read_bytes(path)
  if not bytes then
    return fail("cannot read " .. path .. ": " .. (read_err or "unknown error"))
  end
  local size = buffer.len(bytes)
  if size > MAX_INPUT_BYTES then
    return fail(
      string.format("%s is too large to view (%s; limit %s)", path, format_size(size), format_size(MAX_INPUT_BYTES))
    )
  end

  local info, probe_err = maki.image.probe(bytes)
  if not info then
    return fail(path .. " is not an image (" .. (probe_err or "unrecognized format") .. ")")
  end
  local media_type = MEDIA_TYPES[info.format]
  if not media_type then
    return fail("unsupported image format " .. info.format .. ": only png, jpeg, gif, and webp can be viewed")
  end

  -- Decode fully even on the pass-through path: a corrupt file shipped
  -- undecoded poisons message history and fails every later request.
  local img, decode_err = maki.image.decode(bytes)
  if not img then
    return fail("cannot decode " .. path .. ": " .. (decode_err or "unknown error"))
  end

  if size <= MAX_RAW_BYTES and math.max(info.width, info.height) <= MAX_EDGE then
    return {
      llm_output = caption(path, size, info.width, info.height),
      image = { media_type = media_type, data = maki.base64.encode(bytes) },
    }
  end

  -- Too big for the API: downscale to fit MAX_EDGE and re-encode. JPEG stays
  -- JPEG (photos recompress far smaller); everything else becomes PNG since
  -- gif/webp encoding isn't supported.
  local resized = math.max(info.width, info.height) > MAX_EDGE
  if resized then
    img = img:resize(MAX_EDGE, MAX_EDGE)
  end

  local out_format = info.format == "jpeg" and "jpeg" or "png"
  local encoded = img:encode(out_format)
  if #encoded > MAX_RAW_BYTES and out_format == "png" then
    -- PNG can stay huge at 1568px (e.g. noisy screenshots); JPEG is the only
    -- remaining lever.
    out_format = "jpeg"
    encoded = img:encode(out_format)
  end
  if #encoded > MAX_RAW_BYTES then
    return fail(
      string.format(
        "%s is too large to view (%s after downscaling; limit %s)",
        path,
        format_size(#encoded),
        format_size(MAX_RAW_BYTES)
      )
    )
  end

  local note = resized and string.format(", downscaled from %dx%d", info.width, info.height) or ", re-encoded"
  -- Animated gif/webp lose their animation when re-encoded.
  if info.format == "gif" or info.format == "webp" then
    note = note .. ", first frame only"
  end

  return {
    llm_output = caption(path, #encoded, img:width(), img:height(), note),
    image = { media_type = MEDIA_TYPES[out_format], data = maki.base64.encode(encoded) },
  }
end

maki.api.register_tool({
  name = "view_image",
  effect = "read_only",
  kind = "read",
  description = DESCRIPTION,
  permission_scopes = function(input)
    return { scopes = { maki.fs.normalize(input.path) }, force_prompt = false }
  end,
  -- No interpreter audience: the code_execution bridge flattens tool output
  -- to text, so the pixels could never reach the model from there.
  audiences = { "main", "research_sub", "general_sub" },

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Path to the image file",
        required = true,
        alias = "file_path",
      },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    buf:line({ { shorten_path(input.path or ""), "path" } })
    return buf
  end,

  handler = function(input, _ctx)
    local raw = input.path
    if not raw then
      return fail("error: path is required")
    end
    local path = maki.fs.normalize(raw)
    local meta = maki.fs.metadata(path)
    if not meta then
      return fail("error: path not found: " .. path)
    end
    if meta.is_dir then
      return fail("error: " .. path .. " is a directory")
    end
    return load_image(path)
  end,
})
```

## Full API reference

Every module, function, and method is in the [Lua API reference](/docs/lua-api/).
The agent gets the same document on disk through the builtin
`maki-plugin-dev` skill, so asking it to write a plugin for you works
without pasting any of this.
