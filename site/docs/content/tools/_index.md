+++
title = "Tools"
weight = 4
[extra]
group = "Reference"
+++

# Tools

Caudra ships with 26 built-in tools in this reference (26 on by default, 0 opt-in via plugin options). Tools marked **opt-in** are off until you enable them under `plugins` in [Configuration](/docs/configuration/).

First-party file, web, shell, index, Python, and environment tools run through protocol-neutral Workcell contracts. Workcell owns schemas, validation, execution bounds, atomic file changes, network policy, subprocess cleanup, cancellation, and the bundled worker lifecycle. Caudra owns registration, authorization, retained session output, and model or UI presentation. Release builds pin an exact Workcell revision.

## Disabling tools

`agent.disabled_tools` withholds a tool from the model. Entries are built-in tool names, an MCP tool as `server.tool`, or a whole MCP server as `server.*`. An unknown name fails at startup with the list of valid names. A project list extends the global one, so a project can restrict further and cannot re-enable what the global config turned off.

```lua
caudra.setup({
    agent = { disabled_tools = { "shell", "file_write", "github.*" } },
})
```

`--disallowed-tools` does the same for one run and accepts the same names. `plugins.<name>.enabled = false` still works and maps to the tools that plugin was replaced by, so `plugins.bash` turns off `shell`.

`tool_output` stays available whatever the lists say. The agent calls it on its own to page through a truncated result.

Run [`caudra tools`](/docs/cli/) to see the resulting set, including which rule turned each tool off, or `/tools` inside a session to see it for the open transcript. To keep a tool available but gate every call, use a `deny` or `prompt` default in [Permissions](/docs/permissions/) instead.

## Tools loaded on demand

8 built-in tools start outside the request array. The model sees a `tool_search` entry instead, and one call with a query loads the matching tools for the rest of the session. Sessions that never need them never pay for their descriptions.

`code_map`, `code_context`, `code_refs`, `code_impact`, and `code_expand` load together as the code graph group, because a question about an unfamiliar codebase usually takes several of them in a row.

`execution_environment`, `image_generate`, and `python_execution` load on their own.

Loading changes the tool array, so the provider's prompt cache prefix resets and the next request re-reads the history as fresh input. Caudra posts a notice naming what loaded when it happens.

Listing a tool in `--allowed-tools` asks for it upfront and skips the search. [`caudra tools`](/docs/cli/) marks the rest as `deferred behind tool_search`.

## File Operations

### `file_read` {#file_read}

Read a file or directory from the local filesystem. If the path does not exist, an error is returned.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `filePath` | string | yes | Root-relative or absolute path inside the configured root. |
| `offset` | integer | no | 1-indexed starting line. |
| `limit` | integer | no | Maximum lines to return. |

### `file_write` {#file_write}

Writes a file to the local filesystem.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `filePath` | string | yes | File path inside the configured root. |
| `content` | string | yes | Complete UTF-8 text content. |

### `file_edit` {#file_edit}

Performs exact string replacements in files.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `filePath` | string | yes | File path inside the configured root. |
| `oldString` | string | yes | Exact text to replace. |
| `newString` | string | yes | Replacement text. |
| `replaceAll` | boolean | no | Replace every exact match. |

### `file_apply_patch` {#file_apply_patch}

Use file_apply_patch to edit files with a stripped-down, file-oriented diff format. The patch language is designed to be easy to parse and safe to review.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `patchText` | string | yes | Complete stripped-down file patch. |

### `file_index` {#file_index}

Return a compact structural overview of a source file, or a deterministic listing of a directory.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Root-relative or absolute source file or directory path, limited to 4096 UTF-8 bytes. |

### `file_glob` {#file_glob}

Fast file pattern matching tool for files under the file root.

A search that reaches its bounds returns what it found instead of failing. The result then reports how much was withheld, and the tool card says how far the scan got, so an absent match is distinguishable from an unsearched file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Glob pattern supporting *, **, ?, and brace alternatives. |
| `path` | string | no | Optional directory under the configured root. |

### `file_grep` {#file_grep}

Fast content search tool for files under the file root.

A search that reaches its bounds returns what it found instead of failing. The result then reports how much was withheld, and the tool card says how far the scan got, so an absent match is distinguishable from an unsearched file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Linear-time regular expression without look-around or backreferences. |
| `path` | string | no | Optional file or directory under the root. |
| `include` | string | no | Optional file glob filter. |

### `tool_output` {#tool_output}

Page or search managed tool output owned by the current session. Omit `pattern` to read lines from `offset`, or supply it to return regex matches with context.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `output_id` | string | yes |  | Opaque ID from a tool-output truncation notice. |
| `pattern` | string | no |  | Regex to search for. Omit to read lines instead. |
| `offset` | integer | no | 1 | Starting line, 1-indexed. |
| `limit` | integer | no |  | Lines to return when reading, or matches when searching. Reading defaults to 200 and caps at 2000. Searching defaults to 100 and caps at 200. |
| `byte_offset` | integer | no | 0; use continuation hints | Starting byte within the first line. Reading only. |
| `context_before` | integer | no | 0; capped at 5 | Context lines before each match. Searching only. |
| `context_after` | integer | no | 0; capped at 5 | Context lines after each match. Searching only. |

### `view_image` {#view_image}

View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `file_read` for images.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Path to the image file |

## Code Intelligence

### `code_map` <span class="badge">on demand</span> {#code_map}

Rank every symbol in a source tree by importance and return the top ones. Start here when you do not know a codebase.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | no | Root-relative subdirectory to scope the map to. Absent means the whole configured root, and an empty string is the same as absent. |
| `limit` | integer | no | Maximum rows to return. Narrows the result; it can never widen it past the host ceiling. |

### `code_context` <span class="badge">on demand</span> {#code_context}

Return the symbols worth reading before making a specific change. Describe the change in your own words.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task` | string | yes | The change you are about to make, in your own words. |
| `path` | string | no | Root-relative subdirectory to scope the map to. Absent means the whole configured root, and an empty string is the same as absent. |
| `limit` | integer | no | Maximum rows to return. Narrows the result; it can never widen it past the host ceiling. |

### `code_refs` <span class="badge">on demand</span> {#code_refs}

List the symbols that reference a given symbol, or the symbols it references.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | yes | A symbol name, optionally qualified as `path::name` to disambiguate. |
| `direction` | string | no | `callers` lists symbols referencing this one; `callees` lists the ones it references. |
| `path` | string | no | Root-relative subdirectory to scope the map to. Absent means the whole configured root, and an empty string is the same as absent. |
| `limit` | integer | no | Maximum rows to return. Narrows the result; it can never widen it past the host ceiling. |

### `code_impact` <span class="badge">on demand</span> {#code_impact}

Show what a change to a symbol could reach, and which tests already cover it.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | yes | A symbol name, optionally qualified as `path::name` to disambiguate. |
| `depth` | integer | no | Hops to walk backwards along call edges. Beyond a few hops a reachability set describes the repository rather than a blast radius. |
| `path` | string | no | Root-relative subdirectory to scope the map to. Absent means the whole configured root, and an empty string is the same as absent. |
| `limit` | integer | no | Maximum rows to return. Narrows the result; it can never widen it past the host ceiling. |

### `code_expand` <span class="badge">on demand</span> {#code_expand}

Return one symbol's source together with its immediate callers and callees.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | yes | A symbol name, optionally qualified as `path::name` to disambiguate. |
| `path` | string | no | Root-relative subdirectory to scope the map to. Absent means the whole configured root, and an empty string is the same as absent. |

## Execution & Control

### `batch` {#batch}

Executes multiple independent tool calls concurrently to reduce round-trips.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of tool calls to execute in parallel |

### `shell` {#shell}

Execute a Bash command on the MCP server host.

Caudra shows unfiltered output while the command runs. After completion, the TUI switches to the filtered model-facing result when Workcell reduced it. The output footer names every reduction that ran and toggles between filtered and raw views. Filtering is enabled by default and never changes the reviewed command or structured capture. Set `agent.shell_output_filter = false` or use `--no-rtk` to disable it.

A progress bar redraws a row instead of printing lines. Caudra renders both the live view and the capture as a terminal would show them, so a bar appears as one updating row rather than a single very long line, and the output printed before it is not pushed out of the retained window. Rendering is decoding rather than filtering, so `--no-rtk` does not disable it; the footer reports how many frames were absorbed.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Bash command to execute on the MCP server host. |
| `timeout` | integer | no | Optional timeout in milliseconds. Defaults to 120000 and is capped at 600000. |
| `workdir` | string | no | Optional configured-root-relative or absolute initial working directory inside the configured root. |

### `python_execution` <span class="badge">on demand</span> {#python_execution}

Execute a short Python script in an isolated interpreter and return its value and printed output.

Release builds include the isolated Monty worker. `WORKCELL_MCP_CODE_WORKER` can override it with an operator-supplied worker binary.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `code` | string | yes | Python source to execute. The value of the final expression is returned. |
| `timeout` | integer | no | Optional timeout in milliseconds. Defaults to 5000 and is capped at 30000. |

### `execution_environment` <span class="badge">on demand</span> {#execution_environment}

Inspect the execution host's current sanitized environment.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|

### `question` {#question}

Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `questions` | array | yes | Questions to ask |

## Agent & Knowledge

### `task` {#task}

Launch an autonomous subagent to perform tasks independently. Best combined with batch.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `description` | string | yes | Short (3-5 words) description of the task |
| `prompt` | string | yes | Detailed task prompt for the agent |
| `task_id` | string | no | Set this only to resume. Continues the subagent from an earlier task_id with its existing history instead of starting fresh. |
| `mode` | string | no | Subagent mode. Defaults to "plan" for a new task; omitted continuations retain their stored mode. |
| `profile` | string | no | System prompt profile. Defaults to the parent profile for a new task; use "builtin" explicitly for Caudra's built-in prompt. Omitted continuations retain their stored profile. |
| `output_schema` | string | no | JSON Schema (object) the subagent's final result must match. When set, the result is returned as a validated JSON string. |

### `todo_write` {#todo_write}

Create or update a structured todo list to track tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | The updated todo list |

### `memory` {#memory}

Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | - `list [tags]`: tag-grouped index, no bodies.<br>- `read path\|tags`: one body (path) or collated bodies (tags).<br>- `write path tags content`: create or overwrite a note.<br>- `delete path` |
| `path` | string | no | Relative path, e.g. 'architecture.md'. |
| `content` | string | no | Body for write (frontmatter added automatically). |
| `tags` | array | no | snake_case tags. Filter for list/read; assigned on write (defaults to filename stem). |

### `skill` {#skill}

Load a skill that provides instructions and workflows for specific tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Name of the skill to load |

## Media

### `image_generate` <span class="badge">on demand</span> {#image_generate}

Generate a raster image from a text prompt and save it as a PNG. Use for AI-created bitmap visuals: illustrations, textures, sprites, photos, and mockups. Requires a ChatGPT subscription login (`caudra auth login openai`) and bills against that plan, not API credits.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `prompt` | string | yes | Description of the image to generate. |
| `out` | string | yes | Output file path, relative to the project directory unless absolute. Written as a PNG. |
| `quality` | string | no | Generation quality. Defaults to auto. |
| `size` | string | no | Image size, either `auto` or `WIDTHxHEIGHT`. Width and height must be multiples of 16, the long edge at most 3840, the long-to-short ratio at most 3:1, and the total between 655,360 and 8,294,400 pixels. |
| `images` | array | no | Reference image paths, relative to the project directory unless absolute. |

## Web

### `webfetch` {#webfetch}

Fetch content from a URL and return model-facing text.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `url` | string | yes | HTTP(S) URL to fetch. |
| `format` | string | no | Output format. |
| `pdfMode` | string | no | PDF handling mode. Defaults to extract. |
| `timeout` | integer | no | Timeout in seconds. Defaults to 30, max 60. |

### `websearch` {#websearch}

Search the web using Exa's credential-free hosted MCP service.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | yes | Natural-language search query sent to Exa's hosted MCP service. Maximum 512 characters. |
| `limit` | integer | no | Maximum results to return. Defaults to 10. |
| `timeoutSec` | integer | no | Request timeout in seconds. Defaults to 10. |