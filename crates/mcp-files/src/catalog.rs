#[cfg(feature = "mcp")]
use std::sync::Arc;

#[cfg(feature = "mcp")]
use rmcp::model::{JsonObject, MetaObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use workcell_tool_contract::{ToolAnnotations as NeutralAnnotations, ToolSpec};

#[cfg(feature = "mcp")]
const PRESENTATION_KEY: &str = "ai.workcell/presentation-profile";
const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

const READ_DESCRIPTION: &str = r#"Read a file or directory from the local filesystem. If the path does not exist, an error is returned.

Usage:
- The filePath parameter can be absolute or file-root-relative.
- An empty filePath is treated as "." and reads the file root directory.
- By default, this tool returns up to 2000 lines from the start of the file.
- The offset parameter is the line number to start from (1-indexed).
- To read later sections, call this tool again with a larger offset.
- Use file_grep to find specific content in large files or files with long lines.
- If you are unsure of the correct file path, use file_glob to look up filenames by glob pattern.
- Contents are returned with each line prefixed by its line number as `<line>: <content>`. Directories return sorted entries one per line, with a trailing `/` for subdirectories.
- Any line longer than 2000 characters is truncated, and total model-facing output is byte-limited.
- Avoid tiny repeated slices. If you need more context, read a larger window.
- This first local OSS executor is not sandboxed; hosts can override permission policy before exposing it."#;

const GLOB_DESCRIPTION: &str = r#"Fast file pattern matching tool for files under the file root.

- Supports glob patterns like "**/*.js" or "src/**/*.ts".
- Returns matching file paths with byte sizes and line counts for bounded text files.
- Use this tool when you need to find files by name patterns.
- The optional path parameter limits the search root. Omit it to search the file root.
- An empty path is treated as ".".
- Results skip .git and node_modules and are truncated to a safe result limit.
- When you are doing an open-ended search that may require multiple rounds of globbing and grepping, prefer a higher-level sidequest/task flow once available.
- This first local OSS executor is not sandboxed; hosts can override permission policy before exposing it."#;

const GREP_DESCRIPTION: &str = r#"Fast content search tool for files under the file root.

- Searches file contents using linear-time regular expressions.
- Supports common regex syntax such as alternation, groups, classes, and repetition. Look-around and backreferences are rejected to guarantee bounded matching time.
- Filter files by pattern with the include parameter, for example "*.js" or "*.{ts,tsx}".
- An empty path is treated as ".", and an empty include filter is ignored.
- Returns file paths, line numbers, and matching lines.
- Use this tool when you need to find files containing specific patterns.
- For filtering the output of a shell command pipeline, use shell with rg when available, falling back to grep only when rg is unavailable.
- Results skip binary files, .git, and node_modules and are truncated to a safe match limit.
- This first local OSS executor is not sandboxed; hosts can override permission policy before exposing it."#;

const WRITE_DESCRIPTION: &str = r#"Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path.
- If this is an existing file, use file_read first to understand the current file contents.
- Prefer file_edit for existing files when you are making targeted changes.
- Do not create documentation files unless the user explicitly requested documentation.
- Only use emojis if the user explicitly requests them.
- This tool is mutating and should run only after a permission decision that shows the diff preview."#;

const EDIT_DESCRIPTION: &str = r#"Performs exact string replacements in files.

Usage:
- Use file_read before editing so oldString matches the current file contents.
- When editing text from file_read output, preserve exact indentation after the line number prefix. Never include the line number prefix in oldString or newString.
- Prefer file_edit for existing files. Use file_write only for new files or intentional full rewrites.
- The edit fails if oldString is not found.
- The edit fails if oldString is found multiple times. Provide more surrounding context or use replaceAll for intentional bulk replacements.
- Only use emojis if the user explicitly requests them.
- This tool is mutating and should run only after a permission decision that shows the diff preview."#;

const PATCH_DESCRIPTION: &str = r#"Use file_apply_patch to edit files with a stripped-down, file-oriented diff format. The patch language is designed to be easy to parse and safe to review.

Envelope:
*** Begin Patch
[ one or more file sections ]
*** End Patch

Supported sections:
- *** Add File: <path> creates a new file. Every following line is a + line.
- *** Delete File: <path> deletes an existing file.
- *** Update File: <path> patches an existing file, optionally followed by *** Move to: <path>.

Example:
```
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.ts
@@ const greeting =
-const greeting = "hi";
+const greeting = "hello";
*** End Patch
```

Rules:
- You must include Begin Patch and End Patch markers.
- You must include an action header for each file.
- Prefix new lines with + even when creating a file.
- This tool is mutating and should run only after a permission decision that shows the full patch preview."#;

/// Returns fresh values so a composing server may safely augment its own copy.
/// Order is part of the public compatibility contract and mirrors registration
/// order in the TypeScript MCP server.
#[cfg(feature = "mcp")]
pub fn catalog() -> Vec<Tool> {
    specs().iter().map(to_mcp_tool).collect()
}

#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "file_read",
            "Read file or directory",
            READ_DESCRIPTION,
            read_schema(),
            read_annotations(),
            "file.read.v1",
            "file.read.v1",
        ),
        spec(
            "file_glob",
            "Glob files",
            GLOB_DESCRIPTION,
            glob_schema(),
            read_annotations(),
            "file.list.v1",
            "file.glob.v1",
        ),
        spec(
            "file_grep",
            "Search file contents",
            GREP_DESCRIPTION,
            grep_schema(),
            read_annotations(),
            "file.search.v1",
            "file.grep.v1",
        ),
        spec(
            "file_write",
            "Write file",
            WRITE_DESCRIPTION,
            write_schema(),
            mutation_annotations(true),
            "file.diff.v1",
            "file.write.v1",
        ),
        spec(
            "file_edit",
            "Edit file",
            EDIT_DESCRIPTION,
            edit_schema(),
            mutation_annotations(false),
            "file.diff.v1",
            "file.edit.v1",
        ),
        spec(
            "file_apply_patch",
            "Apply file patch",
            PATCH_DESCRIPTION,
            patch_schema(),
            mutation_annotations(false),
            "file.diff.v1",
            "file.patch.v1",
        ),
    ]
}

fn spec(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input_schema: Map<String, Value>,
    annotations: NeutralAnnotations,
    presentation: &'static str,
    contract_id: &'static str,
) -> ToolSpec {
    ToolSpec::new(
        name,
        Some(title),
        description,
        input_schema,
        annotations,
        presentation,
        contract_id,
    )
}

#[cfg(feature = "mcp")]
fn to_mcp_tool(spec: &ToolSpec) -> Tool {
    let mut meta = JsonObject::new();
    meta.insert(
        PRESENTATION_KEY.to_owned(),
        Value::String(spec.presentation.to_owned()),
    );
    let tool = Tool::new(
        spec.name,
        spec.description.clone(),
        Arc::new(spec.input_schema.clone()),
    );
    let tool = match spec.title {
        Some(title) => tool.with_title(title),
        None => tool,
    };
    tool.with_annotations(ToolAnnotations::from_raw(
        None,
        spec.annotations.read_only_hint,
        spec.annotations.destructive_hint,
        spec.annotations.idempotent_hint,
        spec.annotations.open_world_hint,
    ))
    .with_meta(MetaObject(meta))
}

fn read_annotations() -> NeutralAnnotations {
    NeutralAnnotations {
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(false),
    }
}

fn mutation_annotations(idempotent: bool) -> NeutralAnnotations {
    NeutralAnnotations {
        read_only_hint: Some(false),
        destructive_hint: Some(true),
        idempotent_hint: Some(idempotent),
        open_world_hint: Some(false),
    }
}

fn read_schema() -> Map<String, Value> {
    schema(json!({
        "type": "object",
        "properties": {
            "filePath": {
                "type": "string",
                "minLength": 1,
                "description": "Root-relative or absolute path inside the configured root."
            },
            "offset": {
                "description": "1-indexed starting line.",
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_SAFE_INTEGER
            },
            "limit": {
                "description": "Maximum lines to return.",
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_SAFE_INTEGER
            }
        },
        "required": ["filePath"],
        "$schema": DRAFT_07
    }))
}

fn glob_schema() -> Map<String, Value> {
    schema(json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "minLength": 1,
                "description": "Glob pattern supporting *, **, ?, and brace alternatives."
            },
            "path": {
                "description": "Optional directory under the configured root.",
                "type": "string",
                "minLength": 1
            }
        },
        "required": ["pattern"],
        "$schema": DRAFT_07
    }))
}

fn grep_schema() -> Map<String, Value> {
    schema(json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "minLength": 1,
                "description": "Linear-time regular expression without look-around or backreferences."
            },
            "path": {
                "description": "Optional file or directory under the root.",
                "type": "string",
                "minLength": 1
            },
            "include": {
                "description": "Optional file glob filter.",
                "type": "string",
                "minLength": 1
            }
        },
        "required": ["pattern"],
        "$schema": DRAFT_07
    }))
}

fn write_schema() -> Map<String, Value> {
    schema(json!({
        "type": "object",
        "properties": {
            "filePath": {
                "type": "string",
                "minLength": 1,
                "description": "File path inside the configured root."
            },
            "content": {
                "type": "string",
                "description": "Complete UTF-8 text content."
            },
            "dryRun": {
                "description": "Preview the diff without changing the filesystem.",
                "type": "boolean"
            }
        },
        "required": ["filePath", "content"],
        "$schema": DRAFT_07
    }))
}

fn edit_schema() -> Map<String, Value> {
    schema(json!({
        "type": "object",
        "properties": {
            "filePath": {
                "type": "string",
                "minLength": 1,
                "description": "File path inside the configured root."
            },
            "oldString": {
                "type": "string",
                "minLength": 1,
                "description": "Exact text to replace."
            },
            "newString": {
                "type": "string",
                "description": "Replacement text."
            },
            "replaceAll": {
                "description": "Replace every exact match.",
                "type": "boolean"
            },
            "dryRun": {
                "description": "Preview the diff without changing the filesystem.",
                "type": "boolean"
            }
        },
        "required": ["filePath", "oldString", "newString"],
        "$schema": DRAFT_07
    }))
}

fn patch_schema() -> Map<String, Value> {
    schema(json!({
        "type": "object",
        "properties": {
            "patchText": {
                "type": "string",
                "minLength": 1,
                "description": "Complete stripped-down file patch."
            },
            "dryRun": {
                "description": "Validate and preview the patch without changing files.",
                "type": "boolean"
            }
        },
        "required": ["patchText"],
        "$schema": DRAFT_07
    }))
}

fn schema(value: Value) -> Map<String, Value> {
    value.as_object().expect("schema is an object").clone()
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use serde_json::json;

    use super::{PRESENTATION_KEY, catalog, specs};

    #[test]
    fn catalog_has_compatible_order_annotations_and_metadata() {
        let tools = catalog();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            [
                "file_read",
                "file_glob",
                "file_grep",
                "file_write",
                "file_edit",
                "file_apply_patch"
            ]
        );
        assert_eq!(
            tools[0].annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        assert_eq!(
            tools[3].annotations.as_ref().unwrap().idempotent_hint,
            Some(true)
        );
        assert_eq!(
            tools[0].meta.as_ref().unwrap().0[PRESENTATION_KEY],
            json!("file.read.v1")
        );
        assert_eq!(tools[0].input_schema["$schema"], json!(super::DRAFT_07));

        for (spec, tool) in specs().iter().zip(&tools) {
            assert_eq!(spec.name, tool.name);
            assert_eq!(spec.description, tool.description.as_deref().unwrap());
            assert_eq!(&spec.input_schema, tool.input_schema.as_ref());
            assert_eq!(
                spec.presentation,
                tool.meta.as_ref().unwrap().0[PRESENTATION_KEY]
            );
        }
    }
}
