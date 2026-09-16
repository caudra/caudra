use std::env;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use caudra_agent::permissions::review::review_from_candidates;
use caudra_storage::StateDir;
use caudra_storage::permission_state::{
    PermissionArgumentConstraint, PermissionReviewSource, PermissionRuleRecord, PermissionSubject,
    inventory_fingerprint, validate_conversation_record,
};
use caudra_storage::sessions::{SESSIONS_DB_FILE, SessionDatabase};
use color_eyre::eyre::{Context, Result, bail, eyre};
use serde::Serialize;
use serde_json::{Value, json};

use super::repair::{RULES_KEY, parse_json, tool_name};

const UNAVAILABLE: &str = "Unavailable (no verified preimage)";
const INVALID_REVIEW: &str =
    "stored permissions need review repair; run caudra permissions repair-review";
const APPLICABILITY: &str = "Binding eligibility only; config, requests, deny precedence and tool availability are not evaluated.";

fn terminal_control(character: char) -> bool {
    character.is_control()
        || matches!(character,
            '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}' |
            '\u{890}'..='\u{891}' | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}' |
            '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}' |
            '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' |
            '\u{13430}'..='\u{13455}' | '\u{1bca0}'..='\u{1bca3}' |
            '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e007f}')
}

pub(super) fn terminal_text(text: &str) -> String {
    let mut result = String::new();
    for character in text.chars() {
        if terminal_control(character) {
            result.extend(character.escape_default());
        } else {
            result.push(character);
        }
    }
    result
}

pub(super) fn terminal_json(value: &impl Serialize) -> Result<String> {
    let mut result = String::new();
    for character in serde_json::to_string_pretty(value)?.chars() {
        if !character.is_ascii() && terminal_control(character) {
            for unit in character.encode_utf16(&mut [0; 2]) {
                write!(result, "\\u{unit:04x}")?;
            }
        } else {
            result.push(character);
        }
    }
    Ok(result)
}

fn records(value: Value, persistent: bool) -> Result<Vec<PermissionRuleRecord>> {
    let records: Vec<PermissionRuleRecord> =
        serde_json::from_value(value).map_err(|_| eyre!(INVALID_REVIEW))?;
    if persistent {
        inventory_fingerprint(&records).map_err(|_| eyre!(INVALID_REVIEW))?;
    } else {
        for record in &records {
            validate_conversation_record(record).map_err(|_| eyre!(INVALID_REVIEW))?;
        }
    }
    Ok(records)
}

fn binding(record: &PermissionRuleRecord, project: &Path, session: Option<&str>) -> String {
    if let Some(session) = session {
        return format!("conversation in session {session}");
    }
    match &record.project {
        Some(bound) if bound == project => format!("project {} (current project)", bound.display()),
        Some(bound) => format!("project {} (other project; excluded)", bound.display()),
        None => "global (all projects)".into(),
    }
}

fn render(
    record: &PermissionRuleRecord,
    project: &Path,
    session: Option<&str>,
    candidates: &[String],
) -> Result<String> {
    let recovered = review_from_candidates(
        &record.rule,
        tool_name(&record.rule.subject),
        None,
        candidates,
        PermissionReviewSource::Recovered,
    );
    let review = record.review.as_ref().unwrap_or(&recovered);
    let rule = &record.rule;
    let mut output = String::new();
    writeln!(
        output,
        "{}  {:?} / {:?} / {}",
        terminal_text(&record.id),
        rule.effect,
        rule.lifetime,
        if record.is_active() {
            "active"
        } else {
            "revoked"
        }
    )?;
    writeln!(
        output,
        "  Binding: {}",
        terminal_text(&binding(record, project, session))
    )?;
    writeln!(
        output,
        "  Tool: {} ({:?} executor)",
        terminal_text(&review.tool),
        rule.executor
    )?;
    let subject = match &rule.subject {
        PermissionSubject::Native { owner, .. } => format!("native owner {owner}"),
        PermissionSubject::Lua { plugin, tool, .. } => format!("Lua plugin {plugin}, tool {tool}"),
        PermissionSubject::Mcp {
            server,
            authority,
            tool,
            ..
        } => format!("MCP server {server}, authority {authority}, tool {tool}"),
        PermissionSubject::RemoteWorkcell { identity, tool, .. } => {
            format!("remote Workcell {identity:?}, tool {tool}")
        }
        PermissionSubject::RemoteNative {
            identity, owner, ..
        } => format!("remote native {identity:?}, owner {owner}"),
        PermissionSubject::UnknownLegacy { .. } => {
            "unavailable legacy subject identity; see --json".into()
        }
    };
    writeln!(output, "  Subject: {}", terminal_text(&subject))?;
    writeln!(output, "  Authority: {}", terminal_text(&review.authority))?;
    writeln!(
        output,
        "  Review source: {:?}",
        if record.review.is_some() {
            &review.source
        } else {
            &PermissionReviewSource::Unavailable
        }
    )?;
    for (index, resource) in rule.resources.iter().enumerate() {
        let stored = review
            .resources
            .iter()
            .find(|resource| resource.index == index);
        let known = recovered
            .resources
            .iter()
            .find(|resource| resource.index == index);
        let label = stored
            .and_then(|resource| resource.value.as_deref())
            .or_else(|| known.and_then(|resource| resource.value.as_deref()))
            .unwrap_or(UNAVAILABLE);
        writeln!(
            output,
            "  Scope {}: {}; access {}; protected {}",
            index + 1,
            terminal_text(&format!("{:?}", resource.kind)),
            resource
                .access
                .as_ref()
                .map_or_else(|| "any".into(), |access| format!("{access:?}")),
            resource
                .protected
                .map_or_else(|| "any".into(), |value| value.to_string())
        )?;
        writeln!(output, "    {}", terminal_text(label))?;
        for key in resource.attributes.keys() {
            let label = stored
                .and_then(|resource| resource.attributes.get(key))
                .or_else(|| known.and_then(|resource| resource.attributes.get(key)))
                .map(String::as_str)
                .unwrap_or(UNAVAILABLE);
            writeln!(
                output,
                "    {}: {}",
                terminal_text(key),
                terminal_text(label)
            )?;
        }
    }
    if let Some(input) = &review.input {
        writeln!(
            output,
            "  Input (sanitized; omitted fields remain constrained):"
        )?;
        for line in serde_json::to_string_pretty(input)?.lines() {
            writeln!(output, "    {}", terminal_text(line))?;
        }
    } else if matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained) {
        writeln!(output, "  Input: unconstrained")?;
    } else {
        writeln!(
            output,
            "  Input: unavailable (no verified historical input)"
        )?;
    }
    writeln!(
        output,
        "  Created: {} (Unix seconds){}",
        record.created_at,
        record
            .revoked_at
            .map_or_else(String::new, |at| format!("; revoked: {at}"))
    )?;
    Ok(output)
}

pub(super) fn run(
    state_dir: &StateDir,
    project: Option<PathBuf>,
    mut candidates: Vec<String>,
    json_output: bool,
) -> Result<()> {
    let project = project.map_or_else(env::current_dir, Ok)?;
    if !project.is_absolute() {
        bail!("project must be an absolute path");
    }
    let project_text = project
        .to_str()
        .ok_or_else(|| eyre!("project path is not UTF-8"))?;
    candidates.push(project_text.into());
    let display_path = terminal_text(
        &state_dir
            .path()
            .join(SESSIONS_DB_FILE)
            .display()
            .to_string(),
    );
    if !json_output {
        println!("Database: {display_path}");
    }
    let database = SessionDatabase::open_read_only(state_dir)
        .with_context(|| format!("open permission database {display_path}"))?;
    let snapshot = database.raw_permission_snapshot()?;
    let persistent = snapshot
        .persistent
        .as_deref()
        .map(parse_json)
        .transpose()?
        .map(|value| records(value, true))
        .transpose()?
        .unwrap_or_default();
    let mut conversations = Vec::new();
    for session in &snapshot.sessions {
        if let Some(rules) = parse_json(&session.metadata)?.get(RULES_KEY) {
            conversations.push((session.id.to_string(), records(rules.clone(), false)?));
        }
    }
    if json_output {
        let mut raw_conversations = Vec::new();
        for session in &snapshot.sessions {
            if let Some(rules) = parse_json(&session.metadata)?.get(RULES_KEY) {
                raw_conversations.push(json!({"session": session.id, "rules": rules}));
            }
        }
        println!(
            "{}",
            terminal_json(&json!({
                "read_only": true, "project": project, "applicability": APPLICABILITY,
                "database": database.path(),
                "fingerprint": inventory_fingerprint(&persistent)?,
                "persistent": snapshot.persistent.as_deref().map(parse_json).transpose()?.unwrap_or_else(|| json!([])),
                "conversations": raw_conversations
            }))?
        );
    } else {
        println!(
            "Permission inventory — {} persistent, {} conversation rules",
            persistent.len(),
            conversations
                .iter()
                .map(|(_, rules)| rules.len())
                .sum::<usize>()
        );
        println!("{APPLICABILITY}");
        println!(
            "Logical read-only; SQLite may update existing WAL coordination sidecars. No hashes are inverted.\n"
        );
        for record in &persistent {
            println!("{}", render(record, &project, None, &candidates)?);
        }
        for (session, rules) in &conversations {
            for record in rules {
                println!("{}", render(record, &project, Some(session), &candidates)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INVALID_REVIEW, records, render, terminal_json, terminal_text};
    use caudra_agent::permissions::canonical_json_sha256;
    use caudra_storage::id::CaudraId;
    use caudra_storage::permission_state::PermissionRuleRecord;
    use serde_json::{Value, json};
    use std::path::Path;
    use test_case::test_case;

    const PROJECT: &str = "/old/project";
    const FILE: &str = "/old/project/source.rs";
    const AUTHORITY: &str = "Bound tool; exact input";
    const UNAVAILABLE_INPUT: &str = "Input: unavailable";

    #[test_case("\u{1b}]52;secret\u{7}", "\\u{1b}]52;secret\\u{7}"; "osc")]
    #[test_case("a\nb\rc\td", "a\\nb\\rc\\td"; "whitespace_controls")]
    #[test_case("\u{9b}31m\u{202e}abc", "\\u{9b}31m\\u{202e}abc"; "c1_and_bidi")]
    fn escapes_terminal_controls(input: &str, expected: &str) {
        assert_eq!(terminal_text(input), expected);
    }

    #[test_case("\u{9b}\u{202e}\u{e0001}\u{1b}\n"; "controls_round_trip")]
    fn json_export_escapes_controls_without_changing_values(text: &str) {
        let value = json!({"text": text});
        let encoded = terminal_json(&value).unwrap();
        assert!(encoded.is_ascii());
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), value);
    }

    #[test_case(true; "typed_labels_and_input")]
    #[test_case(false; "honest_missing_input_and_verified_candidate")]
    fn human_inventory_shows_authority_without_hashes(typed: bool) {
        let digest = canonical_json_sha256(&json!(FILE));
        let id = CaudraId::generate().to_string();
        let mut value = json!({"id": id, "created_at": 1, "project": PROJECT,
            "rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "file.read.v1"},
                "executor": "native", "resources": [{"kind": {"kind": "file"}, "selector": {"match": "digest", "digest": digest}, "access": "read", "protected": true}],
                "arguments": {"constraint": "exact", "digest": digest}, "lifetime": "project", "effect": "deny"}});
        if typed {
            value["review"] = json!({"tool": "file_read", "authority": AUTHORITY,
                "resources": [{"index": 0, "value": format!("Exact: {FILE}"), "attributes": {}}],
                "input": {"filePath": FILE, "limit": 17}, "source": "approved"});
        }
        let record: PermissionRuleRecord = serde_json::from_value(value).unwrap();
        let text = render(&record, Path::new(PROJECT), None, &[FILE.into()]).unwrap();
        for expected in [
            &id,
            FILE,
            "Deny",
            "Project",
            "file_read",
            "Read",
            "protected true",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
        assert!(!text.contains(&digest));
        if typed {
            assert!(text.contains(AUTHORITY));
            assert!(text.contains("17"));
        } else {
            assert!(text.contains(UNAVAILABLE_INPUT));
        }
    }

    #[test_case("legacy text"; "legacy_string")]
    fn normal_inventory_rejects_legacy_reviews(review: &str) {
        let value = json!([{"id": CaudraId::generate().to_string(), "created_at": 1,
            "rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "file.read.v1"},
                "executor": "native", "resources": [], "arguments": {"constraint": "unconstrained"}, "lifetime": "global", "effect": "deny"},
            "review": review}]);
        assert_eq!(
            records(value, true).unwrap_err().to_string(),
            INVALID_REVIEW
        );
    }
}
