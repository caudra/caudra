local ToolView = require("caudra.tool_view")
local output_limits = require("caudra.output_limits")

local SMALL_OUTPUT_MAX_BYTES = 8 * 1024
local FALLBACK_MAX_OUTPUT_BYTES = 100 * 1024 * 1024
local CONTROL_RESERVE_BYTES = 256
local CANCELLED_FMT = "[cancelled by user; %s]"
local TIMEOUT_FMT = "[timed out after %ds; %s]"
local PARTIAL_OUTPUT = "output above is partial"
local NO_PARTIAL_OUTPUT = "no output before the cut"
local OUTPUT_LIMIT_MARKER = "[stopped: output limit exceeded]"
local STREAM_FAILURE_MARKER = "[stopped: stream failure]"
local PERSISTENCE_FAILURE_MARKER = "[stopped: output persistence failure]"
local WORKDIR_SCOPE_FMT = "%s # caudra-workdir[%d]=%s # caudra-frame[%d]"
local SEPARATOR = "──────"

local function unquote(s)
  local q = s:sub(1, 1)
  if (q == '"' or q == "'") and s:sub(-1) == q then
    return s:sub(2, -2)
  end
  return s
end

local function parse_cd_hint(input)
  if input.workdir then
    return input.command, input.workdir
  end
  local rest = input.command:match("^cd%s+(.+)$")
  if rest then
    local dir, tail = rest:match("^(.-)%s+&&%s+(.+)$")
    if dir and dir ~= "" then
      return tail, unquote(dir)
    end
  end
  return input.command, nil
end

local function execution(input)
  local command, workdir = parse_cd_hint(input)
  return command, caudra.fs.normalize(workdir or caudra.uv.cwd() or ".")
end

local function normalize_sep(s)
  return s:gsub("\\", "/")
end

local function relative_path(p)
  local np = normalize_sep(p)
  local cwd = caudra.uv.cwd()
  if cwd then
    cwd = normalize_sep(cwd)
    if np:sub(1, #cwd + 1) == cwd .. "/" then
      local rel = np:sub(#cwd + 2)
      return rel == "" and "." or rel
    end
    if np == cwd then
      return "."
    end
  end
  local home = caudra.uv.os_homedir()
  if home then
    home = normalize_sep(home)
    if np:sub(1, #home + 1) == home .. "/" then
      local rel = np:sub(#home + 2)
      return rel == "" and "~" or "~/" .. rel
    end
  end
  return p
end

local function build_header_lines(command)
  local header = {}
  local highlighted = caudra.ui.highlight(command, "bash")
  if highlighted then
    for _, line in ipairs(highlighted) do
      header[#header + 1] = line
    end
  else
    header[#header + 1] = command
  end
  header[#header + 1] = { { SEPARATOR, "dim" } }
  return header
end

local function create_bash_view(command, ctx)
  local tol = ctx:tool_output_lines()
  local buf = caudra.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.bash) or 5,
    keep = "tail",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
  })
  view:set_header(build_header_lines(command))
  buf:on("click", function()
    view:toggle()
  end)
  return buf, view
end

local COMMAND_TYPES = {
  command = true,
  declaration_command = true,
  test_command = true,
  unset_command = true,
}
local FORCE_PROMPT_TYPES = {
  command_substitution = true,
  process_substitution = true,
  subshell = true,
  arithmetic_expansion = true,
}

local function with_workdir(effect, workdir)
  return string.format(WORKDIR_SCOPE_FMT, effect, #workdir, workdir, #workdir)
end

local function redirect_path(raw, workdir)
  local quote = raw:sub(1, 1)
  local quoted = (quote == "'" or quote == '"') and raw:sub(-1) == quote
  local path = quoted and raw:sub(2, -2) or raw
  if path == "" or path:find("[%$`%*%?%[]") then
    return nil
  end
  if not quoted then
    path = path:gsub("\\(.)", "%1")
  end
  if not quoted and (path == "~" or path:sub(1, 2) == "~/") then
    return caudra.fs.normalize(path)
  end
  if path:sub(1, 1) == "/" then
    return caudra.fs.normalize(path)
  end
  return caudra.fs.normalize(caudra.fs.joinpath(workdir, path))
end

local function collect_effects(node, source, workdir, out, seen)
  local kind = node:type()
  if FORCE_PROMPT_TYPES[kind] then
    out.force_prompt = true
  end
  if COMMAND_TYPES[kind] then
    local command = caudra.treesitter.get_node_text(node, source):match("^%s*(.-)%s*$")
    if command ~= "" then
      local scope = with_workdir(command, workdir)
      if not seen[scope] then
        seen[scope] = true
        out[#out + 1] = scope
      end
    end
  elseif kind == "file_redirect" then
    local destinations = node:field("destination")
    local raw = destinations[1] and caudra.treesitter.get_node_text(destinations[1], source) or nil
    if raw then
      local redirect = caudra.treesitter.get_node_text(node, source):match("^%s*(.-)%s*$")
      local descriptor_only = redirect:find("[<>]&") and (raw == "-" or raw:match("^%d+$"))
      if not descriptor_only then
        local target = redirect_path(raw, workdir)
        local scope = with_workdir("redirect " .. redirect .. " => " .. (target or raw), workdir)
        if not seen[scope] then
          seen[scope] = true
          out[#out + 1] = scope
        end
        if not target then
          out.force_prompt = true
        end
      end
    end
  end

  for child in node:iter_children() do
    if child:named() then
      collect_effects(child, source, workdir, out, seen)
    end
  end
end

local function permission_scopes(input)
  local command, workdir = execution(input)
  if not command or command:match("^%s*$") then
    return nil
  end

  local fallback = with_workdir(command, workdir)
  local parser = caudra.treesitter.get_parser(command, "bash")
  if not parser then
    return { scopes = { fallback }, force_prompt = true }
  end

  local root = parser:parse()[1]:root()
  if root:has_error() then
    return { scopes = { fallback }, force_prompt = true }
  end

  local scopes = {}
  collect_effects(root, command, workdir, scopes, {})
  if #scopes == 0 then
    scopes[1] = fallback
  end
  return { scopes = scopes, force_prompt = scopes.force_prompt or false }
end

local description = [[Execute a bash command.
Commands run in the current working directory by default.

- **DO NOT** use for file ops! Only git, builds, tests, and system commands.
- Use `workdir` param instead of `cd <dir> && <cmd>` patterns.
- Do NOT use to communicate text to the user.
- Chain dependent commands with `&&`. Use batch for independent ones.
- Provide a short `description` (3-5 words).
- Output truncated beyond 2000 lines or 50KB.
- Interactive commands (sudo, ssh prompts) fail immediately.]]

caudra.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Reserve bash for system commands (git, builds, tests). Do NOT use bash for file operations, including on files outside the working dir.",
})

local opts = caudra.api.register_options(output_limits.extend({
  timeout_secs = {
    default = 120,
    min = 5,
    desc = "Kill the command after this many seconds. A call's `timeout` param overrides it.",
  },
}))

caudra.api.register_tool({
  name = "bash",
  kind = "execute",
  description = description,
  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "The bash command to execute", required = true },
      timeout = { type = "integer", description = "Timeout in seconds (default 120)" },
      workdir = { type = "string", description = "Working directory (default: cwd)" },
      description = { type = "string", description = "Short description (3-5 words) of what the command does" },
    },
  },
  permission_scopes = permission_scopes,

  header = function(input)
    local command, workdir = parse_cd_hint(input)
    if workdir then
      workdir = caudra.fs.normalize(workdir)
    end
    local s = input.description or command
    if workdir then
      s = s .. " in " .. relative_path(workdir)
    end
    if input.timeout then
      local buf = caudra.ui.buf()
      buf:line({ { s }, { " (" .. caudra.ui.humantime(input.timeout) .. " timeout)", "dim" } })
      return buf
    end
    return s
  end,

  restore = function(input, output, is_error, ctx)
    local command = input.command
    local buf, view = create_bash_view(command, ctx)
    local timeout_secs = output:match("^tool bash timed out after (%d+)s$")
    if timeout_secs then
      view:append({ { "Timed out after " .. timeout_secs .. "s", "dim" } })
    elseif is_error then
      local body, code = output:match("^(.-)\nExit code: (%d+)$")
      if body then
        view:append_text(body)
        view:append({ { "Exit code: " .. code, "dim" } })
      else
        view:append_text(output)
      end
    else
      if output == "Exit code: 0" or output == "" then
        view:clear()
        view:append({ { "No output", "dim" } })
      else
        view:append_text(output)
      end
    end
    view:finish()
    return buf
  end,

  handler = function(input, ctx)
    if not input.command then
      return { llm_output = "error: command is required", is_error = true }
    end

    local command, workdir = execution(input)
    local timeout_secs = input.timeout or opts.timeout_secs
    local max_lines, max_bytes = output_limits.resolve(opts, ctx)
    local limits = { max_lines = max_lines, max_bytes = max_bytes }

    ctx:set_deadline(timeout_secs)

    local buf, view = create_bash_view(command, ctx)

    local sink = ctx:tool_output_sink()
    local output_parts = {}
    local accepted_bytes = 0
    local ends_with_newline = false
    local finished = false
    local job_id
    local view_pending = ""
    local view_pending_stream
    local view_line_fragmented = false

    local function append_part(part, control)
      if part == "" then
        return true
      end
      local next_bytes = accepted_bytes + #part

      if sink then
        local ok, err
        if control then
          ok, err = sink:append_control(part)
        else
          ok, err = sink:append_process_output(part)
        end
        if not ok then
          return nil, err
        end
        if output_parts then
          if next_bytes <= SMALL_OUTPUT_MAX_BYTES then
            output_parts[#output_parts + 1] = part
          else
            output_parts = nil
          end
        end
      else
        local max_bytes = control and FALLBACK_MAX_OUTPUT_BYTES or FALLBACK_MAX_OUTPUT_BYTES - CONTROL_RESERVE_BYTES
        if next_bytes > max_bytes then
          return nil, ("tool output is %d bytes, exceeding the %d-byte limit"):format(next_bytes, max_bytes)
        end
        output_parts[#output_parts + 1] = part
      end

      accepted_bytes = next_bytes
      ends_with_newline = part:sub(-1) == "\n"
      return true
    end

    local function append_control(marker)
      local separator = accepted_bytes > 0 and not ends_with_newline and "\n" or ""
      return append_part(separator .. marker, true)
    end

    local function bounded_prefix_end(text, max_bytes)
      local last = math.min(#text, max_bytes)
      while last > 0 do
        local next_byte = text:byte(last + 1)
        if not next_byte or next_byte < 0x80 or next_byte >= 0xC0 then
          return last
        end
        last = last - 1
      end
      return math.min(#text, max_bytes)
    end

    local function emit_view_fragments()
      local max_bytes = output_limits.DEFAULT_MAX_LINE_BYTES
      while #view_pending > max_bytes do
        local last = bounded_prefix_end(view_pending, max_bytes)
        view:append(view_pending:sub(1, last))
        view_pending = view_pending:sub(last + 1)
        view_line_fragmented = true
      end
    end

    local function flush_view_pending(force_empty)
      if view_pending ~= "" or (force_empty and not view_line_fragmented) then
        view:append(view_pending)
      end
      view_pending = ""
      view_pending_stream = nil
      view_line_fragmented = false
    end

    local function append_view_chunk(chunk, stream)
      if view_pending_stream and view_pending_stream ~= stream then
        flush_view_pending(false)
      end
      view_pending_stream = stream
      local start = 1
      while true do
        local newline = chunk:find("\n", start, true)
        if not newline then
          if start <= #chunk then
            view_pending_stream = stream
            view_pending = view_pending .. chunk:sub(start)
            emit_view_fragments()
          end
          return
        end
        view_pending_stream = stream
        view_pending = view_pending .. chunk:sub(start, newline - 1)
        emit_view_fragments()
        flush_view_pending(true)
        start = newline + 1
      end
    end

    local function finish_stream_error(id, err, stream_error)
      if finished then
        return
      end
      caudra.fn.jobstop(id)

      local is_limit = err:find("exceeding the", 1, true) ~= nil
      local message = is_limit and "Bash output limit exceeded; command stopped: " .. err
        or stream_error and "Bash job stream failed; command stopped: " .. err
        or "Bash output persistence failed; command stopped: " .. err
      if sink then
        local marker = is_limit and OUTPUT_LIMIT_MARKER
          or stream_error and STREAM_FAILURE_MARKER
          or PERSISTENCE_FAILURE_MARKER
        append_control(marker)
      end
      finished = true
      local captured_output = output_parts and table.concat(output_parts) or ""
      flush_view_pending(false)
      view:append({ { message, "dim" } })
      view:finish()

      local reply = {
        llm_output = captured_output ~= "" and captured_output or message,
        is_error = true,
        body = buf,
        output_limits = limits,
        model_suffix = message,
      }
      if sink then
        local managed_output, finish_err = sink:finish()
        sink = nil
        if managed_output then
          reply.llm_output = message
          reply.managed_output = managed_output
        else
          reply.llm_output = message .. "\nFailed to publish accepted output: " .. finish_err
          reply.model_suffix = nil
        end
      end
      ctx:finish(reply)
    end

    local function finish(exit_code)
      if finished then
        return
      end
      local is_error = exit_code ~= 0
      local command_output_empty = accepted_bytes == 0
      if is_error then
        local ok, err = append_control("Exit code: " .. exit_code)
        if not ok then
          finish_stream_error(job_id, err)
          return
        end
      end
      finished = true
      flush_view_pending(false)

      if command_output_empty then
        view:clear()
        view:append({ { "No output", "dim" } })
      end

      if is_error then
        view:append({ { "Exit code: " .. exit_code, "dim" } })
      end
      view:finish()

      local reply = { is_error = is_error, body = buf, output_limits = limits }
      if sink and output_parts then
        local _, discard_err = sink:discard()
        sink = nil
        if discard_err then
          reply.llm_output = "Failed to discard temporary bash output: " .. discard_err
          reply.is_error = true
        else
          local output = table.concat(output_parts)
          reply.llm_output = exit_code == 0 and output == "" and "Exit code: 0" or output
        end
      elseif sink then
        local managed_output, finish_err = sink:finish()
        sink = nil
        if managed_output then
          reply.llm_output = ""
          reply.managed_output = managed_output
        else
          reply.llm_output = "Failed to publish bash output: " .. finish_err
          reply.is_error = true
        end
      else
        local output = table.concat(output_parts)
        reply.llm_output = exit_code == 0 and output == "" and "Exit code: 0" or output
      end
      ctx:finish(reply)
    end

    view:append({ { "Waiting for output...", "dim" } })

    job_id = caudra.fn.jobstart(command, {
      cwd = workdir,
      env = { GIT_TERMINAL_PROMPT = "0" },
      raw_chunks = true,
      on_stdout = function(id, chunk)
        if finished then
          return
        end
        local first = accepted_bytes == 0 and chunk ~= ""
        local ok, err = append_part(chunk, false)
        if not ok then
          finish_stream_error(id, err)
          return
        end
        if first then
          view:clear()
        end
        append_view_chunk(chunk, "stdout")
      end,
      on_stderr = function(id, chunk)
        if finished then
          return
        end
        local first = accepted_bytes == 0 and chunk ~= ""
        local ok, err = append_part(chunk, false)
        if not ok then
          finish_stream_error(id, err)
          return
        end
        if first then
          view:clear()
        end
        append_view_chunk(chunk, "stderr")
      end,
      on_error = function(id, err)
        finish_stream_error(id, err, true)
      end,
      on_exit = function(_, code)
        finish(code)
      end,
    })

    -- Esc or deadline: hand back the lines streamed so far, so the model
    -- keeps what the user just watched instead of a bare error.
    caudra.async.on_cancel(function(reason)
      if finished then
        return
      end
      local partial_tail = accepted_bytes > 0 and PARTIAL_OUTPUT or NO_PARTIAL_OUTPUT
      local marker = reason == "timeout" and TIMEOUT_FMT:format(timeout_secs, partial_tail)
        or CANCELLED_FMT:format(partial_tail)
      local command_output_empty = accepted_bytes == 0
      local ok, err = append_control(marker)
      if not ok then
        finish_stream_error(job_id, err)
        return
      end
      finished = true
      caudra.fn.jobstop(job_id)
      flush_view_pending(false)
      if command_output_empty then
        view:clear()
      end
      view:append({ { marker, "dim" } })
      view:finish()

      local reply = { llm_output = marker, is_error = true, body = buf, output_limits = limits }
      if sink then
        local managed_output, finish_err = sink:finish()
        sink = nil
        if managed_output then
          reply.managed_output = managed_output
        else
          reply.llm_output = (output_parts and table.concat(output_parts) or marker)
            .. "\nFailed to publish partial bash output: "
            .. finish_err
        end
      else
        reply.llm_output = table.concat(output_parts)
      end
      ctx:finish(reply)
    end)

    return nil
  end,
})
