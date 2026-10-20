//! Schema 30: the code-graph tools became the source-code tools. Every stored
//! use of their names, contracts, decision feature and output tag moves to the
//! new spelling, so nothing reads the old one again. Retired tools such as
//! `code_map` keep their names; nothing serves them any more.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Transaction, params, params_from_iter};
use serde_json::Value;
use tracing::warn;

use super::{
    COMPRESSED_PAYLOAD_TABLES, MIGRATION, MigrationEvent, OptionalExtension,
    PERMISSION_METADATA_KEY, PERMISSION_RULES_KEY, PayloadRewrite, STATE_SCOPE_GLOBAL,
    SessionError, compress_payload, decompress_payload, from_i64, to_i64,
};

/// The code-graph tools' old and new names, which the config files move to as
/// well.
pub const SOURCE_CODE_TOOL_RENAMES: [(&str, &str); 2] = [
    ("code_context", "source_code_context"),
    ("code_refs", "source_code_refs"),
];
/// The bare names, plus the two spellings models also wrote: GPT's
/// `functions.` prefix and the Anthropic subscription wire name. Dispatch
/// still strips or maps both, so they move with the bare name.
const RENAMED_TOOLS: [(&str, &str); 6] = [
    SOURCE_CODE_TOOL_RENAMES[0],
    SOURCE_CODE_TOOL_RENAMES[1],
    ("functions.code_context", "functions.source_code_context"),
    ("functions.code_refs", "functions.source_code_refs"),
    ("mcp_Code_context", "mcp_Source_code_context"),
    ("mcp_Code_refs", "mcp_Source_code_refs"),
];
const RENAMED_CONTRACTS: [(&str, &str); 2] = [
    ("code.context.v1", "source_code.context.v1"),
    ("code.refs.v1", "source_code.refs.v1"),
];
const RENAMED_OUTPUT_TAG: (&str, &str) = ("CodeGraph", "SourceCodeGraph");
/// What a payload must contain before it is worth parsing: the end of an old
/// tool name in any of its spellings (`code_`, `Code_`), or the old output
/// tag as a key.
const PAYLOAD_NEEDLES: [&str; 3] = ["ode_context\"", "ode_refs\"", "\"CodeGraph\""];
const PERMISSION_NEEDLES: [&str; 4] = [
    "\"code_context\"",
    "\"code_refs\"",
    "\"code.context.v1\"",
    "\"code.refs.v1\"",
];
const TOOL_OUTPUTS_TABLE: &str = "tool_outputs";
const TOOL_CALL_TYPE: &str = "tool_call";
const BATCH_TOOL: &str = "batch";
const BATCH_TAG: &str = "Batch";
const BATCH_CHILD_NAME_KEYS: [&str; 2] = ["tool", "name"];
const PROGRESS_INTERVAL: u64 = 512;

/// The columns that name a tool or a decision feature. None of them can collide
/// on its key, because the new names were never stored before this step.
pub(super) const COLUMN_RENAMES: &str = r#"
UPDATE tool_ledger SET tool = 'source_code_context' WHERE tool = 'code_context';
UPDATE tool_ledger SET tool = 'source_code_refs' WHERE tool = 'code_refs';
UPDATE session_tool_usage SET tool = 'source_code_context' WHERE tool = 'code_context';
UPDATE session_tool_usage SET tool = 'source_code_refs' WHERE tool = 'code_refs';
UPDATE decisions SET feature = 'source_code_context' WHERE feature = 'code_context';
UPDATE engine_decisions SET feature = 'source_code_context' WHERE feature = 'code_context';
"#;
/// The history search index holds transcript text with the old names, and its
/// cursors would not notice a rewritten item, so it is rebuilt from the
/// renamed transcripts.
const HISTORY_INDEX_RESET: &str =
    "DELETE FROM history_search_docs; DELETE FROM history_search_cursors;";

/// The JSON half of the step: compressed transcripts and tool outputs, then the
/// global and per-session permission rules, then the history search index.
pub(super) fn rewrite_stored(transaction: &Transaction<'_>) -> Result<(), SessionError> {
    for rewrite in &COMPRESSED_PAYLOAD_TABLES {
        let rename = if rewrite.table == TOOL_OUTPUTS_TABLE {
            rename_in_output
        } else {
            rename_in_history_item
        };
        rewrite_payloads(transaction, rewrite, rename)?;
    }
    rewrite_global_rules(transaction)?;
    rewrite_session_rules(transaction)?;
    transaction.execute_batch(HISTORY_INDEX_RESET)?;
    Ok(())
}

/// Streams every payload of one table and keeps only those `rename` changed.
/// The changed rows are few, so they are collected and written after the scan
/// rather than updated under it.
fn rewrite_payloads(
    transaction: &Transaction<'_>,
    rewrite: &PayloadRewrite,
    rename: fn(&mut Value) -> bool,
) -> Result<(), SessionError> {
    let PayloadRewrite {
        table,
        keys,
        placeholders,
        ..
    } = rewrite;
    let key_count = placeholders.matches('?').count();
    let counted: i64 =
        transaction.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
    let total = from_i64(counted, "payload rows")?;
    let mut changed = Vec::new();
    let mut scanned: u64 = 0;
    MIGRATION.report(MigrationEvent::Rewrite {
        table,
        done: 0,
        total,
    });
    {
        let mut select = transaction.prepare(&format!("SELECT {keys}, payload FROM {table}"))?;
        let mut rows = select.query([])?;
        while let Some(row) = rows.next()? {
            scanned += 1;
            if scanned.is_multiple_of(PROGRESS_INTERVAL) {
                MIGRATION.report(MigrationEvent::Rewrite {
                    table,
                    done: scanned,
                    total,
                });
            }
            let stored: Vec<u8> = row.get(key_count)?;
            let payload = decompress_payload(&stored, "payload")?;
            if !PAYLOAD_NEEDLES
                .iter()
                .any(|needle| payload.contains(needle))
            {
                continue;
            }
            let mut value: Value = match serde_json::from_str(&payload) {
                Ok(value) => value,
                Err(error) => {
                    warn!(table, %error, "left an unreadable payload unrenamed");
                    continue;
                }
            };
            if !rename(&mut value) {
                continue;
            }
            let mut key = Vec::with_capacity(key_count);
            for index in 0..key_count {
                key.push(row.get::<_, SqlValue>(index)?);
            }
            changed.push((key, value.to_string()));
        }
    }
    MIGRATION.report(MigrationEvent::Rewrite {
        table,
        done: scanned,
        total,
    });
    let matches_key = keys
        .split(", ")
        .enumerate()
        .map(|(index, column)| format!("{column} = ?{}", index + 3))
        .collect::<Vec<_>>()
        .join(" AND ");
    let mut update = transaction.prepare(&format!(
        "UPDATE {table} SET payload = ?1, byte_count = ?2 WHERE {matches_key}"
    ))?;
    let rewritten = changed.len();
    for (key, payload) in changed {
        let mut values = vec![
            SqlValue::Blob(compress_payload(&payload)?),
            SqlValue::Integer(to_i64(payload.len(), "payload bytes")?),
        ];
        values.extend(key);
        update.execute(params_from_iter(values))?;
    }
    tracing::info!(
        table,
        scanned,
        rewritten,
        "renamed source-code tools in payloads"
    );
    Ok(())
}

fn rewrite_global_rules(transaction: &Transaction<'_>) -> Result<(), SessionError> {
    let stored: Option<String> = transaction
        .query_row(
            "SELECT value FROM state WHERE scope = ?1 AND key = ?2",
            params![STATE_SCOPE_GLOBAL, PERMISSION_RULES_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(renamed) = stored.as_deref().and_then(rename_in_rules) else {
        return Ok(());
    };
    transaction.execute(
        "UPDATE state SET value = ?3 WHERE scope = ?1 AND key = ?2",
        params![STATE_SCOPE_GLOBAL, PERMISSION_RULES_KEY, renamed],
    )?;
    tracing::info!("renamed source-code tools in global permission rules");
    Ok(())
}

fn rewrite_session_rules(transaction: &Transaction<'_>) -> Result<(), SessionError> {
    let path = format!("$.{PERMISSION_METADATA_KEY}");
    let mut changed = Vec::new();
    {
        let mut select = transaction
            .prepare("SELECT id, metadata -> ?1 FROM sessions WHERE metadata -> ?1 IS NOT NULL")?;
        let mut rows = select.query([&path])?;
        while let Some(row) = rows.next()? {
            let rules: String = row.get(1)?;
            if let Some(renamed) = rename_in_rules(&rules) {
                changed.push((row.get::<_, Vec<u8>>(0)?, renamed));
            }
        }
    }
    let rewritten = changed.len();
    for (id, rules) in changed {
        transaction.execute(
            "UPDATE sessions SET metadata = json_set(metadata, ?2, json(?3)) WHERE id = ?1",
            params![id, path, rules],
        )?;
    }
    tracing::info!(
        rewritten,
        "renamed source-code tools in session permission rules"
    );
    Ok(())
}

/// A permission inventory with every renamed contract and tool moved, or
/// `None` when it names neither. An unreadable inventory is left for the
/// permission loader to report, which it already does.
fn rename_in_rules(rules: &str) -> Option<String> {
    if !PERMISSION_NEEDLES
        .iter()
        .any(|needle| rules.contains(needle))
    {
        return None;
    }
    let mut records: Value = serde_json::from_str(rules).ok()?;
    let mut changed = false;
    for record in records.as_array_mut().into_iter().flatten() {
        if let Some(subject) = record.pointer_mut("/rule/subject") {
            changed |= rename_field(subject, "contract", &RENAMED_CONTRACTS);
            changed |= rename_field(subject, "tool", &RENAMED_TOOLS);
        }
        if let Some(review) = record.get_mut("review") {
            changed |= rename_field(review, "tool", &RENAMED_TOOLS);
        }
    }
    changed.then(|| records.to_string())
}

/// A transcript item: a tool call's name, and for `batch` the name of every
/// call it carries.
fn rename_in_history_item(item: &mut Value) -> bool {
    if item.get("type").and_then(Value::as_str) != Some(TOOL_CALL_TYPE) {
        return false;
    }
    if rename_field(item, "name", &RENAMED_TOOLS) {
        return true;
    }
    if item.get("name").and_then(Value::as_str) != Some(BATCH_TOOL) {
        return false;
    }
    let mut changed = false;
    let calls = item
        .pointer_mut("/input/tool_calls")
        .and_then(Value::as_array_mut);
    for call in calls.into_iter().flatten() {
        for key in BATCH_CHILD_NAME_KEYS {
            changed |= rename_field(call, key, &RENAMED_TOOLS);
        }
    }
    changed
}

/// A stored tool output: the externally tagged variant itself, or the tool and
/// output of every entry a batch holds.
fn rename_in_output(output: &mut Value) -> bool {
    let Some(object) = output.as_object_mut() else {
        return false;
    };
    let (old_tag, new_tag) = RENAMED_OUTPUT_TAG;
    if object.len() == 1
        && let Some(inner) = object.remove(old_tag)
    {
        object.insert(new_tag.to_owned(), inner);
        return true;
    }
    let entries = object
        .get_mut(BATCH_TAG)
        .and_then(|batch| batch.get_mut("entries"))
        .and_then(Value::as_array_mut);
    let mut changed = false;
    for entry in entries.into_iter().flatten() {
        changed |= rename_field(entry, "tool", &RENAMED_TOOLS);
        if let Some(inner) = entry.get_mut("output") {
            changed |= rename_in_output(inner);
        }
    }
    changed
}

fn rename_field(object: &mut Value, key: &str, renames: &[(&str, &str)]) -> bool {
    let Some(field) = object.get_mut(key) else {
        return false;
    };
    let Some((_, new)) = renames.iter().find(|(old, _)| field.as_str() == Some(*old)) else {
        return false;
    };
    *field = Value::from(*new);
    true
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{rename_in_history_item, rename_in_output, rename_in_rules};

    #[test_case(
        json!({"type": "tool_call", "name": "code_refs", "input": {"symbol": "code_refs"}}),
        Some(json!({"type": "tool_call", "name": "source_code_refs", "input": {"symbol": "code_refs"}}));
        "single_call_keeps_its_input"
    )]
    #[test_case(
        json!({"type": "tool_call", "name": "batch", "input": {"tool_calls": [
            {"tool": "code_context", "parameters": {"task": "code_refs"}},
            {"name": "code_refs"},
            {"tool": "functions.code_refs"},
            {"tool": "code_map"},
        ]}}),
        Some(json!({"type": "tool_call", "name": "batch", "input": {"tool_calls": [
            {"tool": "source_code_context", "parameters": {"task": "code_refs"}},
            {"name": "source_code_refs"},
            {"tool": "functions.source_code_refs"},
            {"tool": "code_map"},
        ]}}));
        "batch_children"
    )]
    #[test_case(json!({"type": "tool_call", "name": "code_map"}), None; "retired_tool")]
    #[test_case(json!({"type": "tool_result", "content": "code_refs"}), None; "result_text")]
    #[test_case(json!({"type": "user", "name": "code_refs"}), None; "not_a_call")]
    fn history_items(mut item: Value, expected: Option<Value>) {
        let changed = rename_in_history_item(&mut item);
        assert_eq!(changed.then_some(item), expected);
    }

    #[test_case(
        json!({"CodeGraph": {"headline": "callers"}}),
        Some(json!({"SourceCodeGraph": {"headline": "callers"}}));
        "card"
    )]
    #[test_case(
        json!({"Batch": {"entries": [
            {"tool": "code_refs", "output": {"CodeGraph": {"rows": []}}},
            {"tool": "mcp_Code_context", "output": {"CodeGraph": {"rows": []}}},
            {"tool": "file_read", "output": {"Plain": {"text": "CodeGraph"}}},
            {"tool": "code_expand", "output": {"CodeGraph": {"rows": []}}},
        ], "text": ""}}),
        Some(json!({"Batch": {"entries": [
            {"tool": "source_code_refs", "output": {"SourceCodeGraph": {"rows": []}}},
            {"tool": "mcp_Source_code_context", "output": {"SourceCodeGraph": {"rows": []}}},
            {"tool": "file_read", "output": {"Plain": {"text": "CodeGraph"}}},
            {"tool": "code_expand", "output": {"SourceCodeGraph": {"rows": []}}},
        ], "text": ""}}));
        "batch_entries"
    )]
    #[test_case(json!({"Plain": {"text": "\"CodeGraph\""}}), None; "text_mention")]
    fn outputs(mut output: Value, expected: Option<Value>) {
        let changed = rename_in_output(&mut output);
        assert_eq!(changed.then_some(output), expected);
    }

    #[test_case(
        json!([{"rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "code.refs.v1"}},
                "review": {"tool": "code_refs"}}]),
        Some(json!([{"rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "source_code.refs.v1"}},
                     "review": {"tool": "source_code_refs"}}]));
        "native_rule_and_review"
    )]
    #[test_case(
        json!([{"rule": {"subject": {"kind": "remote_workcell", "tool": "code_context", "contract": "code.context.v1"}}}]),
        Some(json!([{"rule": {"subject": {"kind": "remote_workcell", "tool": "source_code_context", "contract": "source_code.context.v1"}}}]));
        "remote_workcell_rule"
    )]
    #[test_case(
        json!([{"rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "code.map.v1"}},
                "review": {"tool": "code_map"}}]),
        None;
        "retired_contract"
    )]
    fn permission_rules(rules: Value, expected: Option<Value>) {
        let renamed = rename_in_rules(&rules.to_string());
        assert_eq!(
            renamed.map(|text| serde_json::from_str::<Value>(&text).unwrap()),
            expected
        );
    }
}
