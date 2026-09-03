local ToolView = require("caudra.tool_view")
local helpers = require("memory_helpers")
local ListPicker = require("caudra.list_picker")

local MEMORY_POLICY_TOOLS = { "memory", "file_write", "file_edit", "file_apply_patch" }

local function memories_path_suffix()
  local cwd = caudra.uv.cwd()
  local root = caudra.fs.root(cwd, ".git") or cwd
  return "projects/" .. helpers.project_id(root) .. "/memories"
end

-- Notes live outside cwd, where effectful tools normally prompt.
local function register_memory_rules()
  local state = caudra.env.state_dir()
  if not state then
    return
  end
  local dir = caudra.fs.normalize(caudra.fs.joinpath(state, memories_path_suffix()))
  for _, tool in ipairs(MEMORY_POLICY_TOOLS) do
    caudra.api.register_permission_rule({ tool = tool, scope = dir .. "/**" })
  end
end
register_memory_rules()

local function resolve_dir()
  local state = caudra.env.state_dir()
  if not state then
    return nil, "cannot resolve state dir"
  end
  return caudra.fs.joinpath(state, memories_path_suffix())
end

caudra.api.register_prompt_hint({
  prompt = "system",
  slot = "after_instructions",
  content = function()
    local dir = resolve_dir()
    if not dir then
      return nil
    end
    local tag_line = helpers.format_tag_line(dir, helpers.MAX_TAGS)
    if not tag_line then
      return nil
    end
    return '\n\nMemory tags (`memory` with `command="read"` and `tags=[...]`): ' .. tag_line .. "\n"
  end,
})

caudra.api.register_prompt_hint({
  prompt = { "system", "general" },
  slot = "tool_usage",
  content = "- Proactively save non-obvious project gotchas and architecture decisions to **memory**.",
})

local function render_content(content, path, ctx)
  local buf = caudra.ui.buf()
  local tol = ctx:tool_output_lines()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.other) or 20,
    keep = "head",
  })
  buf:on("click", function()
    view:toggle()
  end)

  local ext = path:match("%.([^%.]+)$") or "md"
  if not view:set_highlight(content, ext) then
    view:append_text(content)
  end
  view:finish()
  return buf
end

local function cmd_read(path, dir, ctx)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  local content, err = caudra.fs.read(file_path)
  if not content then
    return nil, "read error: " .. err
  end
  local formatted =
    helpers.cap_read_output(helpers.format_read_entry(path, #content, content), helpers.CAP_HINT_REWRITE)
  return {
    llm_output = formatted,
    body = render_content(formatted, path, ctx),
  }
end

local function cmd_write(path, content, tags, dir, ctx)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end

  local size_err = helpers.validate_write_size(content)
  if size_err then
    return nil, size_err
  end
  local normalized, tag_err, note = helpers.validate_write_tags(tags or {})
  if tag_err then
    return nil, tag_err
  end
  local full = helpers.encode_frontmatter(normalized) .. content
  caudra.fs.mkdir(dir, { parents = true })
  local ok, write_err = caudra.fs.write(file_path, full)
  if not ok then
    return nil, "write error: " .. tostring(write_err)
  end
  return {
    llm_output = "wrote "
      .. path
      .. " (tags: "
      .. (#normalized > 0 and table.concat(normalized, ", ") or "none")
      .. ")"
      .. (note and ("; " .. note) or ""),
    body = render_content(content, path, ctx),
  }
end

local function cmd_delete(path, dir)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  if not caudra.fs.metadata(file_path) then
    return nil, "'" .. path .. "' does not exist"
  end
  local ok, rm_err = caudra.fs.rm(file_path)
  if not ok then
    return nil, "delete error: " .. tostring(rm_err)
  end
  return "deleted " .. path
end

local function with_dir(res, dir)
  local prefix = "dir: " .. dir .. "\n\n"
  if type(res) == "string" then
    return prefix .. res
  end
  res.llm_output = prefix .. res.llm_output
  return res
end

local function memory_permission_scopes(input)
  local command = input.command
  local dir = resolve_dir()
  if not dir then
    return nil
  end
  local scope = caudra.fs.normalize(dir)
  if input.path then
    scope = helpers.safe_resolve(scope, input.path) or scope
  else
    scope = scope:sub(-1) == "/" and (scope .. "**") or (scope .. "/**")
  end
  return { scopes = { scope }, force_prompt = false }
end

caudra.api.register_tool({
  name = "memory",
  effect = "mutating",
  permission_scopes = memory_permission_scopes,
  description = "Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.\n\n"
    .. "- Notes are retrieved by tag; reuse the tags from your system prompt when they fit.\n"
    .. "- Save important context before compaction or to build up project knowledge.\n"
    .. "- Keep entries concise and current. Delete outdated information.\n"
    .. "- The memory `list` and `read` commands report the notes dir; use `file_edit` on `<dir>/<name>` for targeted changes.",

  schema = {
    type = "object",
    properties = {
      command = {
        type = "string",
        enum = { "list", "read", "write", "delete" },
        description = "- `list [tags]`: tag-grouped index, no bodies.\n"
          .. "- `read path|tags`: one body (path) or collated bodies (tags).\n"
          .. "- `write path tags content`: create or overwrite a note.\n"
          .. "- `delete path`",
        required = true,
      },
      path = {
        type = "string",
        description = "Relative path, e.g. 'architecture.md'.",
      },
      content = { type = "string", description = "Body for write (frontmatter added automatically)." },
      tags = {
        type = "array",
        items = { type = "string" },
        description = "snake_case tags. Filter for list/read; assigned on write (defaults to filename stem).",
      },
    },
  },

  header = function(input)
    local parts = { input.command or "" }
    if input.path then
      parts[#parts + 1] = input.path
    elseif input.tags then
      parts[#parts + 1] = table.concat(input.tags, ",")
    end
    return table.concat(parts, " ")
  end,

  restore = function(input, output, _is_error, ctx)
    local content = (input.command == "write" and input.content) or output
    return render_content(content, input.path or "memory.md", ctx)
  end,

  handler = function(input, ctx)
    if type(input.tags) == "string" then
      input.tags = { input.tags }
    end
    local verr = helpers.validate_input(input)
    if verr then
      return { llm_output = "error: " .. verr, is_error = true }
    end
    local cmd = input.command
    local dir, dir_err = resolve_dir()
    if not dir then
      return { llm_output = "error: " .. dir_err, is_error = true }
    end

    local result, err
    if cmd == "list" then
      result, err = helpers.format_list(dir, input.tags)
    elseif cmd == "read" then
      if input.tags and #input.tags > 0 then
        result, err = helpers.format_read(dir, input.tags)
      else
        result, err = cmd_read(input.path, dir, ctx)
      end
    elseif cmd == "write" then
      result, err = cmd_write(input.path, input.content, input.tags, dir, ctx)
    elseif cmd == "delete" then
      result, err = cmd_delete(input.path, dir)
    end
    if err then
      return { llm_output = "error: " .. err, is_error = true }
    end
    if cmd == "list" or cmd == "read" then
      return with_dir(result, dir)
    end
    return result
  end,
})

local function popup_build_items(dir)
  local groups, warnings = helpers.grouped_tags(dir)
  local items = {}
  for _, g in ipairs(groups) do
    for _, f in ipairs(g.files) do
      items[#items + 1] = {
        label = f.name,
        detail = "(" .. f.size .. " bytes)",
        section = g.tag,
        section_detail = "(" .. #g.files .. ")",
      }
    end
  end
  return items, warnings
end

caudra.api.register_command({
  name = "/memory",
  description = "View, edit, and delete memory files",
  handler = function()
    local dir = resolve_dir()
    if not dir then
      caudra.ui.flash("Cannot resolve memory directory")
      return
    end

    local items, warnings = popup_build_items(dir)
    if #items == 0 then
      caudra.ui.flash("No memories yet")
      return
    end
    if #warnings > 0 then
      caudra.ui.flash(#warnings .. " unreadable memory file(s)")
    end
    local last_cursor = 1
    while true do
      if last_cursor > #items then
        last_cursor = math.max(1, #items)
      end
      local event = ListPicker.open(items, {
        title = " Memory Files ",
        cursor = last_cursor,
        submit_keys = { "ctrl+o" },
        footer = {
          { "Enter", "open" },
          { "Ctrl+O", "edit" },
          { "Ctrl+D", "delete" },
        },
      })

      if event.type == "close" then
        break
      end

      last_cursor = event.index
      if event.type == "choice" then
        local item = items[event.index]
        if item then
          local path = caudra.fs.joinpath(dir, item.label)
          local code = caudra.ui.open_editor(path)
          if code == 0 then
            items = popup_build_items(dir)
          end
        end
      elseif event.type == "delete" then
        local item = items[event.index]
        local ok, err = caudra.fs.rm(caudra.fs.joinpath(dir, item.label))
        if ok then
          caudra.ui.flash("Deleted " .. item.label)
          items = popup_build_items(dir)
          if #items == 0 then
            break
          end
        else
          caudra.ui.flash("Delete failed: " .. tostring(err))
        end
      else
        break
      end
    end
  end,
})
