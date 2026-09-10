-- Structured-output story: the subagent gets a session-local structured_output
-- tool whose handler validates and captures the result as closure upvalues.
-- Invalid input is an inline tool error the model can fix in the same run.
-- This plugin owns structured output and subagent concurrency; Rust exposes
-- primitives only (`caudra.agent.session`, `caudra.json.schema_validator`,
-- `caudra.async.semaphore`).
--
-- It also owns the /tasks picker over the subagents spawned here: picker.lua
-- registers the command and the keymap when this file is loaded, so the two
-- cannot be enabled apart and left pointing at each other's absence.

local ToolView = require("caudra.tool_view")
local output_limits = require("caudra.output_limits")
require("picker")

local STRUCTURED_OUTPUT_NAME = "structured_output"
local STRUCTURED_OUTPUT_DESCRIPTION = "Report your final result. Call it exactly once when your task is complete."
local STRUCTURED_OUTPUT_ACK = "Output recorded."
local STRUCTURED_OUTPUT_PROMPT_SUFFIX = "\n\nWhen finished, call the structured_output tool with your final result."
local MAX_NUDGES = 2
local MAX_SCHEMA_ERRORS = 3
local SCHEMA_COMPILE_ERROR = "invalid output_schema"
local SCHEMA_ROOT_ERROR = "output_schema must have type object"
local STRUCTURED_MISSING_ERROR = "subagent finished without calling structured_output"
local STRUCTURED_INVALID_ERROR = "subagent result does not match output_schema"
local SUMMARY_MISSING_ERROR = "subagent finished without providing a summary"
local TASK_METADATA_FORMAT = "<task_metadata>\ntask_id: %s\n</task_metadata>"
local NUDGE_MISSING =
  "You did not call the structured_output tool. Call it now with your final result matching its input schema."
local NUDGE_SUMMARY =
  "You finished your work but did not provide a summary. Reply with a concise summary of what you did and found."
local INVALID_INPUT_PREFIX =
  "Input does not match the required schema. Fix the errors and call structured_output again:\n"
local BODY_INDENT_COLS = 4
local MIN_MD_WIDTH = 20
local DEFAULT_OUTPUT_LINES = 5

local description = [[Launch an autonomous subagent to perform tasks independently. Best combined with batch.

Modes:
- `plan` (default): Strictly read-only. For exploration, review, and implementation planning.
- `build`: Can modify files and run commands. For implementation work.

Available system prompt profiles:
{task_system_prompt_profiles}

Notes:
1. Launch multiple tasks concurrently when possible.
2. The agent's result is not visible to the user. Summarize it in your response.
3. A fresh call gives the subagent no context beyond your prompt, so make the prompt self-contained and state exactly what to report back.
4. Every result, success or failure, carries a task_id. Pass it back to continue that subagent with its previous messages and tool outputs, sending only the new work. Omit mode and profile when continuing; they stay locked to the original run.
5. Tell it to return concise summaries with file:line refs, not full file contents.
]]

local opts = caudra.api.register_options({
  max_concurrent = { default = 8, min = 1, desc = "Max concurrently running subagents." },
})

local schema = {
  type = "object",
  required = { "description", "prompt" },
  additionalProperties = false,
  ["x-caudra-reject-unknown"] = true,
  properties = {
    description = {
      type = "string",
      description = "Short (3-5 words) description of the task",
    },
    prompt = {
      type = "string",
      description = "Detailed task prompt for the agent",
    },
    task_id = {
      type = "string",
      description = "Set this only to resume. Continues the subagent from an earlier task_id with its existing history instead of starting fresh.",
    },
    mode = {
      type = "string",
      enum = { "plan", "build" },
      description = 'Subagent mode. Defaults to "plan" for a new task; omitted continuations retain their stored mode.',
    },
    profile = {
      type = "string",
      description = 'System prompt profile. Defaults to the parent profile for a new task; use "builtin" explicitly for Caudra\'s built-in prompt. Omitted continuations retain their stored profile.',
    },
    output_schema = {
      description = "JSON Schema (object) the subagent's final result must match. When set, the result is returned as a validated JSON string.",
    },
  },
}

local examples = {
  {
    description = "Find auth middleware",
    prompt = "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.",
  },
}

-- Process-wide cap on concurrent subagents.
local semaphore = caudra.async.semaphore(opts.max_concurrent)

local function bounded_errors(errors)
  local out = {}
  for i = 1, math.min(#errors, MAX_SCHEMA_ERRORS) do
    out[i] = errors[i]
  end
  return table.concat(out, "\n")
end

local function with_task_id(task_id, reply)
  reply.model_suffix = string.format(TASK_METADATA_FORMAT, task_id)
  return reply
end

local function handler(input, ctx)
  -- Compile early: a bad schema costs zero tokens.
  local validator
  if input.output_schema then
    if type(input.output_schema) ~= "table" or input.output_schema.type ~= "object" then
      return { llm_output = SCHEMA_ROOT_ERROR, is_error = true }
    end
    local compile_err
    validator, compile_err = caudra.json.schema_validator(input.output_schema)
    if compile_err then
      return { llm_output = SCHEMA_COMPILE_ERROR .. ": " .. compile_err, is_error = true }
    end
  end

  local captured, last_errors
  local local_tools
  if validator then
    local_tools = {
      [STRUCTURED_OUTPUT_NAME] = {
        description = STRUCTURED_OUTPUT_DESCRIPTION,
        input_schema = input.output_schema,
        effect = "read_only",
        handler = function(value)
          local errs = validator:validate(value)
          if errs then
            last_errors = bounded_errors(errs)
            return nil, INVALID_INPUT_PREFIX .. last_errors
          end
          captured = value
          return STRUCTURED_OUTPUT_ACK
        end,
      },
    }
  end

  local permit = semaphore:acquire()
  local sess

  -- pcall so a raised error cannot leak the permit.
  local ok, out = pcall(function()
    local sess_err
    sess, sess_err = caudra.agent.session(ctx, {
      task = true,
      task_id = input.task_id,
      profile = input.profile,
      mode = input.mode,
      local_tools = local_tools,
      name = input.description,
    })
    if sess_err then
      return { llm_output = sess_err, is_error = true }
    end
    local task_id = sess:id()

    local message = input.prompt
    if validator then
      message = message .. STRUCTURED_OUTPUT_PROMPT_SUFFIX
    end

    local result, err = sess:prompt(message)
    local retries = 0
    while not err and retries < MAX_NUDGES do
      if validator and not captured then
        retries = retries + 1
        result, err = sess:prompt(NUDGE_MISSING)
      elseif not validator and result.text == "" then
        retries = retries + 1
        result, err = sess:prompt(NUDGE_SUMMARY)
      else
        break
      end
    end

    sess:close()

    if err then
      -- A result alongside the error means the run was cut short after
      -- streaming some text, and half a transcript beats a bare error.
      if result then
        return with_task_id(task_id, {
          llm_output = "sub-agent interrupted (" .. err .. "). Partial output:\n" .. result.text,
          is_error = true,
        })
      end
      return with_task_id(task_id, { llm_output = "sub-agent error: " .. err, is_error = true })
    end
    if validator and not captured then
      local msg = last_errors and (STRUCTURED_INVALID_ERROR .. ":\n" .. last_errors) or STRUCTURED_MISSING_ERROR
      return with_task_id(task_id, { llm_output = msg, is_error = true })
    end
    if not validator and result.text == "" then
      return with_task_id(task_id, { llm_output = SUMMARY_MISSING_ERROR, is_error = true })
    end
    return with_task_id(task_id, {
      llm_output = captured and caudra.json.encode(captured) or result.text,
      format = "markdown",
    })
  end)

  permit:release()
  if not ok then
    if sess then
      local task_id = sess:id()
      pcall(function()
        sess:close()
      end)
      return with_task_id(task_id, { llm_output = "sub-agent error: " .. tostring(out), is_error = true })
    end
    error(out, 0)
  end
  return out
end

local function header(input)
  return input.description
end

-- Standalone runs render markdown on the Rust side (format = "markdown");
-- this mirrors that for restore and batch children, which build the body here.
local function restore(_input, output, is_error, ctx)
  local tol = ctx:tool_output_lines()
  return ToolView.restore_markdown(output, is_error, {
    max_lines = (tol and tol.task) or DEFAULT_OUTPUT_LINES,
    keep = "head",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
    width = math.max(caudra.ui.terminal_size().cols - BODY_INDENT_COLS, MIN_MD_WIDTH),
  })
end

caudra.api.register_tool({
  name = "task",
  effect = "orchestrator",
  description = description,
  kind = "execute",
  audiences = { "main" },
  examples = examples,
  schema = schema,
  handler = handler,
  header = header,
  restore = restore,
})
