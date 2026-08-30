local DEFAULT_READ_OFFSET = 1
local DEFAULT_READ_BYTE_OFFSET = 0
local DEFAULT_READ_LIMIT = 200
local DEFAULT_GREP_OFFSET = 1
local DEFAULT_GREP_LIMIT = 100
local DEFAULT_CONTEXT = 0
local MAX_READ_LIMIT = 2000
local MAX_GREP_LIMIT = 200
local MAX_CONTEXT = 5
local MAX_OUTPUT_BYTES = 50 * 1024
local MAX_OUTPUT_LINES = 2000

local function line_count(text)
  if text == "" then
    return 0
  end
  local _, newlines = text:gsub("\n", "\n")
  return newlines + 1
end

local function fits(output)
  return #output <= MAX_OUTPUT_BYTES and line_count(output) <= MAX_OUTPUT_LINES
end

local function read_hint(output_id, offset, byte_offset, limit)
  if byte_offset > 0 then
    return string.format(
      "Next call: tool_output_read(output_id=%q, offset=%d, byte_offset=%d, limit=%d)",
      output_id,
      offset,
      byte_offset,
      limit
    )
  end
  return string.format("Next call: tool_output_read(output_id=%q, offset=%d, limit=%d)", output_id, offset, limit)
end

local function format_read(output_id, result, limit)
  local lines = {}
  if result.returned_lines > 0 then
    lines = maki.split(result.text, "\n")
    while #lines > result.returned_lines do
      table.remove(lines)
    end
  end

  while true do
    local next_offset = result.next_offset
    local next_byte_offset = result.next_byte_offset or 0
    if #lines < result.returned_lines then
      next_offset = result.offset + #lines
      next_byte_offset = 0
    end

    local metadata
    if #lines == 0 then
      metadata = string.format(
        "Tool output %s: no lines returned from offset %d; %d total lines (%d bytes)",
        output_id,
        result.offset,
        result.total_lines,
        result.total_bytes
      )
    else
      metadata = string.format(
        "Tool output %s: lines %d-%d of %d (%d bytes)",
        output_id,
        result.offset,
        result.offset + #lines - 1,
        result.total_lines,
        result.total_bytes
      )
    end

    local parts = { metadata }
    if #lines > 0 then
      parts[#parts + 1] = ""
      for _, line in ipairs(lines) do
        parts[#parts + 1] = line
      end
    end
    if next_offset then
      parts[#parts + 1] = ""
      parts[#parts + 1] = read_hint(output_id, next_offset, next_byte_offset, limit)
    end
    local output = table.concat(parts, "\n")
    if fits(output) or #lines == 0 then
      return output
    end
    table.remove(lines)
  end
end

local function grep_hint(output_id, pattern, offset, limit, context_before, context_after)
  return string.format(
    "Next call: tool_output_grep(output_id=%q, pattern=%q, offset=%d, limit=%d, context_before=%d, context_after=%d)",
    output_id,
    pattern,
    offset,
    limit,
    context_before,
    context_after
  )
end

local function format_grep(output_id, pattern, result, limit, context_before, context_after)
  if #result.rows == 0 then
    return "No matches."
  end

  local parts = {}
  for _, row in ipairs(result.rows) do
    local indicator = row.is_match and ":" or "-"
    parts[#parts + 1] = string.format("%d%s %s", row.line_number, indicator, row.text)
  end
  if result.next_offset then
    parts[#parts + 1] = ""
    parts[#parts + 1] = grep_hint(output_id, pattern, result.next_offset, limit, context_before, context_after)
  end
  return table.concat(parts, "\n")
end

local function error_result(err)
  return { llm_output = "error: " .. tostring(err), is_error = true }
end

maki.api.register_tool({
  name = "tool_output_read",
  effect = "read_only",
  kind = "read",
  description = "Read a page of managed tool output owned by the current session.",
  audiences = { "all" },
  schema = {
    type = "object",
    properties = {
      output_id = { type = "string", description = "Opaque ID from a tool-output truncation notice.", required = true },
      offset = { type = "integer", description = "Starting line, 1-indexed (default: 1)." },
      byte_offset = {
        type = "integer",
        description = "Starting byte within the first line (default: 0; use continuation hints).",
      },
      limit = { type = "integer", description = "Maximum lines to return (default: 200; capped at 2000)." },
    },
  },
  handler = function(input, ctx)
    local offset = input.offset or DEFAULT_READ_OFFSET
    local byte_offset = input.byte_offset or DEFAULT_READ_BYTE_OFFSET
    local limit = math.min(input.limit or DEFAULT_READ_LIMIT, MAX_READ_LIMIT)
    local result, err = ctx:tool_output_read(input.output_id, offset, limit, byte_offset)
    if not result then
      return error_result(err)
    end
    return format_read(input.output_id, result, limit)
  end,
})

maki.api.register_tool({
  name = "tool_output_grep",
  effect = "read_only",
  kind = "search",
  description = "Search managed tool output owned by the current session using a regex.",
  audiences = { "all" },
  schema = {
    type = "object",
    properties = {
      output_id = { type = "string", description = "Opaque ID from a tool-output truncation notice.", required = true },
      pattern = { type = "string", description = "Regex pattern.", required = true },
      offset = { type = "integer", description = "Starting line, 1-indexed (default: 1)." },
      limit = { type = "integer", description = "Maximum matches to return (default: 100; capped at 200)." },
      context_before = { type = "integer", description = "Context lines before each match (default: 0; capped at 5)." },
      context_after = { type = "integer", description = "Context lines after each match (default: 0; capped at 5)." },
    },
  },
  handler = function(input, ctx)
    local offset = input.offset or DEFAULT_GREP_OFFSET
    local limit = math.min(input.limit or DEFAULT_GREP_LIMIT, MAX_GREP_LIMIT)
    local context_before = math.min(input.context_before or DEFAULT_CONTEXT, MAX_CONTEXT)
    local context_after = math.min(input.context_after or DEFAULT_CONTEXT, MAX_CONTEXT)
    local result, err =
      ctx:tool_output_grep(input.output_id, input.pattern, offset, limit, context_before, context_after)
    if not result then
      return error_result(err)
    end
    return format_grep(input.output_id, input.pattern, result, limit, context_before, context_after)
  end,
})
