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
- Broad searches skip .git, node_modules, and regenerable build output and tool caches such as target, dist, .venv, and __pycache__. Dependency sources such as vendor are not skipped. Pass one of the skipped directories as the path to search inside it.
- Results are truncated to a safe result limit. A truncated result says so on its last line and reports how many files matched in total.
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
- Broad searches skip binary files, .git, node_modules, and regenerable build output and tool caches such as target, dist, .venv, and __pycache__. Dependency sources such as vendor are not skipped. Pass one of the skipped directories as the path to search inside it.
- Results are truncated to a safe match limit. A truncated result says so on its last line and reports how many files were searched.
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

#[cfg(feature = "index")]
const INDEX_DESCRIPTION: &str = r#"Return a compact structural overview of a source file, or a deterministic listing of a directory.

- Use this first to understand file structure before reading specific sections with file_read.
- File results include imports, declarations, signatures, source ranges, parse recovery status, and semantic output-line metadata.
- Directory results list directories before files and append / to directory names.
- The path may be absolute or relative to the configured file root.
- Unsupported file types, binary files, and non-UTF-8 source are rejected.
- Parsing, traversal, source, lines, output, admission, and concurrency are bounded by host-only policy."#;

/// Returns fresh values so a composing server may safely augment its own copy.
/// Order is part of the public compatibility contract and mirrors registration
/// order in the TypeScript MCP server.
///
/// `allow_write` must be the write authority of the group that will serve these
/// tools. Advertising a mutation tool a call can never satisfy would spend model
/// turns on a guaranteed failure, so the read-only catalog omits them entirely.
#[cfg(feature = "mcp")]
pub fn catalog(allow_write: bool) -> Vec<Tool> {
    specs(allow_write).iter().map(to_mcp_tool).collect()
}

#[must_use]
pub fn specs(allow_write: bool) -> Vec<ToolSpec> {
    let mut specs = vec![
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
    ];
    if allow_write {
        specs.extend([
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
        ]);
    }
    append_index_spec(specs)
}

#[cfg(feature = "index")]
fn append_index_spec(mut specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
    specs.push(
        spec(
            "index",
            "Index source file or directory",
            INDEX_DESCRIPTION,
            index_schema(),
            read_annotations(),
            "file.index.v1",
            "file.index.v1",
        )
        .with_output_schema(index_output_schema()),
    );
    specs
}

#[cfg(not(feature = "index"))]
fn append_index_spec(specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
    specs
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
    let tool = match &spec.output_schema {
        Some(output_schema) => tool.with_raw_output_schema(Arc::new(output_schema.clone())),
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
            }
        },
        "required": ["filePath", "content"],
        "additionalProperties": false,
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
            }
        },
        "required": ["filePath", "oldString", "newString"],
        "additionalProperties": false,
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
            }
        },
        "required": ["patchText"],
        "additionalProperties": false,
        "$schema": DRAFT_07
    }))
}

#[cfg(feature = "index")]
fn index_schema() -> Map<String, Value> {
    schema(json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "description": format!(
                    "Root-relative or absolute source file or directory path, limited to {} UTF-8 bytes.",
                    crate::INDEX_MAX_PATH_BYTES
                )
            }
        },
        "required": ["path"],
        "additionalProperties": false,
        "$schema": DRAFT_07
    }))
}

#[cfg(feature = "index")]
fn index_output_schema() -> Map<String, Value> {
    let source_range = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["startLine", "endLine"],
        "properties": {
            "startLine": {"type": "integer", "minimum": 1},
            "endLine": {"type": "integer", "minimum": 1}
        }
    });
    let output_line = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["outputLine", "text", "semantic"],
        "properties": {
            "outputLine": {"type": "integer", "minimum": 1},
            "text": {"type": "string"},
            "semantic": {
                "type": "string",
                "enum": ["section", "item", "dimmed", "plain"]
            },
            "body": {"type": "string"},
            "sourceRange": source_range
        }
    });
    let directory_entry = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["name", "kind"],
        "properties": {
            "name": {"type": "string"},
            "kind": {"type": "string", "enum": ["directory", "file"]}
        }
    });
    schema(json!({
        "$schema": DRAFT_07,
        "type": "object",
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "kind", "path", "relativePath", "language", "lines",
                    "sourceLineCount", "parseError", "truncated"
                ],
                "properties": {
                    "kind": {"const": "file"},
                    "path": {"type": "string"},
                    "relativePath": {"type": "string"},
                    "language": {"type": "string"},
                    "lines": {"type": "array", "items": output_line},
                    "sourceLineCount": {"type": "integer", "minimum": 1},
                    "parseError": {"type": "boolean"},
                    "truncated": {"type": "boolean"}
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "kind", "path", "relativePath", "entries", "totalCount", "truncated"
                ],
                "properties": {
                    "kind": {"const": "directory"},
                    "path": {"type": "string"},
                    "relativePath": {"type": "string"},
                    "entries": {"type": "array", "items": directory_entry},
                    "totalCount": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Exact when truncated is false; otherwise a lower bound on visible entries processed before scanning stopped."
                    },
                    "truncated": {"type": "boolean"}
                }
            }
        ]
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
        let tools = catalog(true);
        let expected = vec![
            "file_read",
            "file_glob",
            "file_grep",
            "file_write",
            "file_edit",
            "file_apply_patch",
        ];
        #[cfg(feature = "index")]
        let expected = expected.into_iter().chain(["index"]).collect::<Vec<_>>();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            expected
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

        for (spec, tool) in specs(true).iter().zip(&tools) {
            assert_eq!(spec.name, tool.name);
            assert_eq!(spec.description, tool.description.as_deref().unwrap());
            assert_eq!(&spec.input_schema, tool.input_schema.as_ref());
            assert_eq!(
                spec.presentation,
                tool.meta.as_ref().unwrap().0[PRESENTATION_KEY]
            );
        }
        #[cfg(feature = "index")]
        {
            let index = specs(true).pop().expect("index spec");
            assert_eq!(index.name, "index");
            assert_eq!(index.contract_id, "file.index.v1");
            assert_eq!(index.presentation, "file.index.v1");
            assert_eq!(index.output_schema.as_ref().unwrap()["type"], "object");
            let variants = index.output_schema.as_ref().unwrap()["oneOf"]
                .as_array()
                .expect("index output variants");
            assert!(
                !variants[0]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("skeleton"))
            );
            assert!(
                !variants[1]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("listing"))
            );
            let path = &index.input_schema["properties"]["path"];
            assert!(path.get("maxLength").is_none());
            assert_eq!(
                path["description"],
                json!(format!(
                    "Root-relative or absolute source file or directory path, limited to {} UTF-8 bytes.",
                    crate::INDEX_MAX_PATH_BYTES
                ))
            );
        }
    }

    #[test]
    fn read_only_catalog_omits_every_mutation_tool_and_keeps_order() {
        let names = catalog(false)
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        let expected = vec!["file_read", "file_glob", "file_grep"];
        #[cfg(feature = "index")]
        let expected = expected.into_iter().chain(["index"]).collect::<Vec<_>>();
        assert_eq!(names, expected);
        assert_eq!(specs(false).len(), names.len());
    }

    #[test]
    fn mutation_schemas_reject_unknown_arguments() {
        // A stale `dryRun` argument must fail loudly rather than be dropped into
        // an unintended write.
        for spec in specs(true)
            .into_iter()
            .filter(|spec| spec.name.starts_with("file_") && spec.name != "file_read")
            .filter(|spec| spec.annotations.read_only_hint == Some(false))
        {
            assert_eq!(
                spec.input_schema["additionalProperties"],
                json!(false),
                "{} must reject unknown arguments",
                spec.name
            );
            assert!(spec.input_schema["properties"].get("dryRun").is_none());
        }
    }
}
