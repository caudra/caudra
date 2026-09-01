local dir_listing = require("caudra.dir_listing")
local list_helpers = require("list_helpers")
local shorten_path = require("caudra.shorten_path")

local DESCRIPTION =
  [[List directory contents. Returns entry names sorted alphabetically, directories first with a trailing /.

- Filters out instruction files (AGENTS.md, CLAUDE.md, COPILOT.md).]]

caudra.api.register_prompt_hint({
  slot = "tool_usage",
  content = [[
- Use the **list** tool to see what a directory contains.]],
})

caudra.api.register_tool({
  name = "list",
  kind = "read",
  description = DESCRIPTION,
  permission_scopes = function(input)
    return { scopes = { caudra.fs.normalize(input.path) }, force_prompt = false }
  end,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the directory",
        required = true,
      },
    },
  },

  header = function(input)
    local buf = caudra.ui.buf()
    buf:line({ { shorten_path(input.path or ""), "path" } })
    return buf
  end,

  restore = function(_input, output, _is_error, ctx)
    return dir_listing.view(output, ctx)
  end,

  handler = function(input, ctx)
    local result = list_helpers.handler(input, ctx)
    if not result.is_error then
      result.body = dir_listing.view(result.llm_output, ctx)
    end
    return result
  end,
})
