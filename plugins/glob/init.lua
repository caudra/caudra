local ToolView = require("caudra.tool_view")
local shorten_path = require("caudra.shorten_path")
local output_limits = require("caudra.output_limits")

local NO_FILES_FOUND = "No files found"

local opts = caudra.api.register_options(output_limits.extend({
  search_result_limit = { default = 100, min = 10, desc = "Max files returned per search." },
}))

local function search_path(input)
  return caudra.fs.normalize(input.path or ".")
end

local function search_scope(input)
  local path = search_path(input)
  return path:sub(-1) == "/" and (path .. "**") or (path .. "/**")
end

local function glob_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 3, keep = "head" }
end

caudra.api.register_tool({
  name = "glob",
  kind = "search",
  description = [[Find files by glob pattern.

- Respects .gitignore.
- Returns absolute paths sorted by modification time (newest first).
- Prefer speculative parallel searches over sequential rounds of glob+grep.]],
  permission_scopes = function(input)
    return { scopes = { search_scope(input) }, force_prompt = false }
  end,

  schema = {
    type = "object",
    properties = {
      pattern = { type = "string", description = "Glob pattern (e.g. **/*.rs, src/**/*.ts)", required = true },
      path = { type = "string", description = "Directory to search in (default: cwd)" },
    },
  },

  header = function(input)
    local buf = caudra.ui.buf()
    local spans = { { shorten_path(input.pattern or ""), "tool" } }
    if input.path then
      spans[#spans + 1] = { " in ", "dim" }
      spans[#spans + 1] = { shorten_path(input.path), "path" }
    end
    buf:line(spans)
    return buf
  end,

  restore = function(_input, output, _is_error, ctx)
    return ToolView.restore(output, glob_view_opts(ctx))
  end,

  handler = function(input, ctx)
    local pattern = input.pattern
    if not pattern then
      return { llm_output = "error: pattern is required", is_error = true }
    end

    local limit = opts.search_result_limit
    local max_lines, max_bytes = output_limits.resolve(opts, ctx)
    local limits = { max_lines = max_lines, max_bytes = max_bytes }

    local files, err = caudra.fs.glob(pattern, {
      path = search_path(input),
      gitignore = true,
      sort = "mtime",
      limit = limit,
    })

    if not files then
      return { llm_output = "error: " .. err, is_error = true, output_limits = limits }
    end

    if #files == 0 then
      return { llm_output = NO_FILES_FOUND, output_limits = limits }
    end

    local lines = {}
    for i, f in ipairs(files) do
      lines[i] = shorten_path(f)
    end
    local text = table.concat(lines, "\n")

    local buf = caudra.ui.buf()
    local view = ToolView.new(buf, glob_view_opts(ctx))
    for _, line in ipairs(lines) do
      view:append(line)
    end
    view:finish()
    buf:on("click", function()
      view:toggle()
    end)

    return {
      llm_output = text,
      body = buf,
      output_limits = limits,
    }
  end,
})
