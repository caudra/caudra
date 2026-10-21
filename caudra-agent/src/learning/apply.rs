//! What Caudra does with a learner's report: checks every item against the
//! rules a model cannot be trusted to keep, applies what passes, and records
//! each change so it can be reverted. The model never writes a file.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use caudra_config::LearningTarget;
use caudra_storage::history_search::HistoryRef;
use caudra_storage::id::CaudraId;
use caudra_storage::learning::LearningEdit;
use caudra_storage::memory_journal::EntryOrigin;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use super::targets::{
    BLOCK_CLOSE, BLOCK_OPEN, InstructionFile, MAX_BULLET_BYTES, MAX_BULLETS, Written, bullets,
    ensure_ignored,
};
use crate::memory::store::{MemoryError, MemoryStore, journal_body, note_name};
use crate::permissions::review::redact_text;

pub const MAX_NOTE_BYTES: usize = 4 * 1024;
/// Distinct sessions a new bullet of the shared, committed file must cite.
pub const PROJECT_SESSIONS: usize = 2;
pub const EMPTY: &str = "empty";
pub const MULTI_LINE: &str = "a bullet is one line";
pub const TOO_LONG: &str = "too long";
pub const SECRET: &str = "holds text that looks secret";
pub const MARKER: &str = "holds a block marker";
pub const NO_EVIDENCE: &str = "cites no history of this project";
pub const ONE_SESSION: &str = "a shared rule needs evidence from two sessions";
pub const OVER_CAP: &str = "over the bullet cap";
pub const UNAVAILABLE: &str = "target not enabled here";
pub const CONFLICT: &str = "the file changed while the run was learning";
pub const BAD_NAME: &str = "not a valid note name";
pub const DUPLICATE: &str = "repeats another bullet";
const COMMENT_OPEN: &str = "<!--";

/// The shape a learner reports in, given to it as its `output_schema`.
pub static OUTPUT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let evidence = json!({"type": "array", "items": {"type": "string"}});
    let block = json!({
        "type": ["object", "null"],
        "properties": {
            "bullets": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}, "evidence": evidence},
                    "required": ["text"],
                },
            },
        },
        "required": ["bullets"],
    });
    json!({
        "type": "object",
        "properties": {
            "memory": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "content": {"type": "string"},
                        "evidence": evidence,
                    },
                    "required": ["name", "content", "evidence"],
                },
            },
            "instructions": {
                "type": "object",
                "properties": {"project": block, "local": block, "global": block},
            },
            "summary": {"type": "string"},
        },
        "required": ["summary"],
    })
});

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub memory: Vec<NoteItem>,
    #[serde(default)]
    pub instructions: Blocks,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NoteItem {
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// `None` leaves a target's block as it is.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Blocks {
    pub project: Option<Block>,
    pub local: Option<Block>,
    pub global: Option<Block>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Block {
    #[serde(default)]
    pub bullets: Vec<Bullet>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bullet {
    pub text: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl Report {
    /// Every reference the report cites, for [`Evidence`] to resolve.
    pub fn cited(&self) -> HashSet<String> {
        let notes = self.memory.iter().flat_map(|note| &note.evidence);
        let bullets = self
            .blocks()
            .flat_map(|(_, block)| &block.bullets)
            .flat_map(|bullet| &bullet.evidence);
        notes.chain(bullets).cloned().collect()
    }

    fn blocks(&self) -> impl Iterator<Item = (LearningTarget, &Block)> {
        let Blocks {
            project,
            local,
            global,
        } = &self.instructions;
        [
            (LearningTarget::Project, project),
            (LearningTarget::Local, local),
            (LearningTarget::Global, global),
        ]
        .into_iter()
        .filter_map(|(target, block)| Some((target, block.as_ref()?)))
    }
}

/// The cited references that resolve in this project's history, each to its
/// session.
#[derive(Debug, Clone, Default)]
pub struct Evidence(HashMap<String, CaudraId>);

impl Evidence {
    pub fn new(resolved: impl IntoIterator<Item = HistoryRef>) -> Self {
        Self(
            resolved
                .into_iter()
                .map(|reference| (reference.to_string(), reference.session))
                .collect(),
        )
    }

    /// The distinct sessions `cited` reaches, and the references that did.
    fn sessions(&self, cited: &[String]) -> (usize, Vec<String>) {
        let resolved: Vec<&String> = cited.iter().filter(|r| self.0.contains_key(*r)).collect();
        let sessions: HashSet<&CaudraId> = resolved.iter().filter_map(|r| self.0.get(*r)).collect();
        (sessions.len(), resolved.into_iter().cloned().collect())
    }
}

/// An item Caudra refused, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dropped {
    pub target: String,
    pub item: String,
    pub reason: String,
}

impl Dropped {
    fn new(target: LearningTarget, item: &str, reason: &str) -> Self {
        Self {
            target: target.as_str().to_owned(),
            item: item.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotePlan {
    pub name: String,
    pub content: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPlan {
    pub target: LearningTarget,
    pub bullets: Vec<String>,
    pub evidence: Vec<String>,
}

/// What passed validation, and what did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub notes: Vec<NotePlan>,
    pub blocks: Vec<BlockPlan>,
    pub dropped: Vec<Dropped>,
}

/// Checks the report against the files the learner was shown and the
/// targets enabled here. An invalid item is dropped and the rest stand.
pub fn validate(
    report: &Report,
    files: &[InstructionFile],
    memory_enabled: bool,
    evidence: &Evidence,
) -> Plan {
    let mut plan = Plan::default();
    for (target, block) in report.blocks() {
        let Some(file) = files.iter().find(|file| file.target == target) else {
            plan.dropped.extend(
                block
                    .bullets
                    .iter()
                    .map(|bullet| Dropped::new(target, &bullet.text, UNAVAILABLE)),
            );
            continue;
        };
        plan.blocks.push(validate_block(
            target,
            block,
            &file.bullets(),
            evidence,
            &mut plan.dropped,
        ));
    }
    for note in &report.memory {
        let target = LearningTarget::Memory;
        let content = note.content.trim();
        let reason = match () {
            _ if !memory_enabled => Some(UNAVAILABLE),
            _ if note_name(&note.name).is_err() => Some(BAD_NAME),
            _ if content.is_empty() => Some(EMPTY),
            _ if content.len() > MAX_NOTE_BYTES => Some(TOO_LONG),
            _ if redact_text(content) != content => Some(SECRET),
            _ => None,
        };
        let (sessions, cited) = evidence.sessions(&note.evidence);
        match reason.or((sessions == 0).then_some(NO_EVIDENCE)) {
            Some(reason) => plan.dropped.push(Dropped::new(target, &note.name, reason)),
            None => plan.notes.push(NotePlan {
                name: note.name.clone(),
                content: format!("{content}\n"),
                evidence: cited,
            }),
        }
    }
    plan
}

fn validate_block(
    target: LearningTarget,
    block: &Block,
    existing: &[String],
    evidence: &Evidence,
    dropped: &mut Vec<Dropped>,
) -> BlockPlan {
    let mut plan = BlockPlan {
        target,
        bullets: Vec::new(),
        evidence: Vec::new(),
    };
    for bullet in &block.bullets {
        let text = bullet.text.trim().trim_start_matches("- ").trim();
        let kept = existing.iter().any(|old| old == text);
        let (sessions, cited) = evidence.sessions(&bullet.evidence);
        let needed = match target {
            LearningTarget::Project => PROJECT_SESSIONS,
            _ => 1,
        };
        let reason = match () {
            _ if text.is_empty() => Some(EMPTY),
            _ if text.contains(['\n', '\r']) => Some(MULTI_LINE),
            _ if text.len() > MAX_BULLET_BYTES => Some(TOO_LONG),
            _ if [BLOCK_OPEN, BLOCK_CLOSE, COMMENT_OPEN]
                .iter()
                .any(|marker| text.contains(marker)) =>
            {
                Some(MARKER)
            }
            _ if redact_text(text) != text => Some(SECRET),
            _ if plan.bullets.iter().any(|other| other == text) => Some(DUPLICATE),
            _ if kept => None,
            _ if sessions == 0 => Some(NO_EVIDENCE),
            _ if sessions < needed => Some(ONE_SESSION),
            _ => None,
        };
        let reason = reason.or((plan.bullets.len() >= MAX_BULLETS).then_some(OVER_CAP));
        match reason {
            Some(reason) => dropped.push(Dropped::new(target, text, reason)),
            None => {
                plan.bullets.push(text.to_owned());
                plan.evidence.extend(cited);
            }
        }
    }
    plan
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Storage(#[from] caudra_storage::StorageError),
    #[error("a learned edit names an unknown target `{0}`")]
    Target(String),
}

/// What applying a plan changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub edits: Vec<LearningEdit>,
    /// Targets left alone because they changed while the run was learning.
    pub conflicts: Vec<LearningTarget>,
}

impl Applied {
    pub fn count(&self, target: LearningTarget) -> usize {
        self.edits
            .iter()
            .filter(|edit| edit.target == target.as_str())
            .count()
    }
}

/// Applies the plan: each block through a compare-and-write against the file
/// the learner was shown, each note through the journal as `run`'s. The local
/// file is kept out of version control before anything is written to it.
pub fn apply(
    plan: &Plan,
    files: &[InstructionFile],
    memory: Option<&MemoryStore>,
    run: i64,
) -> Result<Applied, ApplyError> {
    let mut applied = Applied::default();
    for block in &plan.blocks {
        let Some(file) = files.iter().find(|file| file.target == block.target) else {
            continue;
        };
        if file.bullets() == block.bullets {
            continue;
        }
        let after = file.rewritten(&block.bullets);
        if block.target == LearningTarget::Local {
            ensure_ignored(&file.path)?;
        }
        match file.write_if_unchanged(&after)? {
            Written::Conflict => applied.conflicts.push(block.target),
            Written::Wrote => applied.edits.push(LearningEdit {
                target: block.target.as_str().to_owned(),
                path: file.path.display().to_string(),
                before: file.content.clone(),
                after: Some(after),
                evidence: evidence_json(&block.evidence),
            }),
        }
    }
    let Some(memory) = memory else {
        return Ok(applied);
    };
    for note in &plan.notes {
        let before = note_body(memory, &note.name)?;
        if memory
            .write(&note.name, &note.content, &EntryOrigin::Learner(run))?
            .is_some()
        {
            applied.edits.push(LearningEdit {
                target: LearningTarget::Memory.as_str().to_owned(),
                path: note_name(&note.name)?,
                before,
                after: Some(note.content.clone()),
                evidence: evidence_json(&note.evidence),
            });
        }
    }
    Ok(applied)
}

/// How a revert went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reverted {
    Reverted,
    /// The file or note changed since the run wrote it, and was left alone.
    Changed,
}

/// Undoes one edit, provided what it wrote is still there: a block gets its
/// old bullets back, a note its old body, and a note the run created goes.
pub fn revert(
    edit: &LearningEdit,
    memory: Option<&MemoryStore>,
    origin: &EntryOrigin,
) -> Result<Reverted, ApplyError> {
    let target = LearningTarget::ALL
        .into_iter()
        .find(|target| target.as_str() == edit.target)
        .ok_or_else(|| ApplyError::Target(edit.target.clone()))?;
    if target == LearningTarget::Memory {
        let Some(memory) = memory else {
            return Ok(Reverted::Changed);
        };
        let current = note_body(memory, &edit.path)?;
        let written = edit.after.as_deref().map(journal_body);
        if current.as_deref().map(journal_body) != written {
            return Ok(Reverted::Changed);
        }
        match &edit.before {
            Some(before) => memory.write(&edit.path, before, origin)?,
            None => memory.delete(&edit.path, origin)?,
        };
        return Ok(Reverted::Reverted);
    }
    let file = InstructionFile::read(target, edit.path.clone().into())
        .map_err(caudra_storage::StorageError::from)?;
    let written = edit.after.as_deref().map(bullets).unwrap_or_default();
    if file.bullets() != written {
        return Ok(Reverted::Changed);
    }
    let before = edit.before.as_deref().map(bullets).unwrap_or_default();
    Ok(match file.write_if_unchanged(&file.rewritten(&before))? {
        Written::Wrote => Reverted::Reverted,
        Written::Conflict => Reverted::Changed,
    })
}

fn note_body(memory: &MemoryStore, name: &str) -> Result<Option<String>, MemoryError> {
    let name = note_name(name)?;
    let current = memory.journal().current(memory.scope())?;
    Ok(current
        .into_iter()
        .find(|entry| entry.meta.name == name)
        .map(|entry| entry.body))
}

fn evidence_json(evidence: &[String]) -> String {
    serde_json::to_string(evidence).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use caudra_storage::history_search::HistoryStream;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::learning::targets::target_path;

    const RULE: &str = "Run cargo nextest, never cargo test";
    const OTHER_RULE: &str = "Regenerate docs with make gen-docs";
    const SECRET_RULE: &str = "Use OPENAI_API_KEY=sk-live-0123456789abcdefghijklmnop";
    const NOTE: &str = "testing.md";
    const NOTE_BODY: &str = "# Testing\n\nnextest is the runner.";
    const HAND_WRITTEN: &str = "# Project\n";
    const GITIGNORE: &str = ".gitignore";
    const IGNORE_LINE: &str = "AGENTS.local.md\n";

    fn reference(session: CaudraId) -> HistoryRef {
        HistoryRef {
            session,
            stream: HistoryStream::Main,
            item: CaudraId::generate(),
        }
    }

    /// Two references from one session and one from another.
    fn evidence() -> (Evidence, Vec<String>) {
        let first = CaudraId::generate();
        let refs = [
            reference(first),
            reference(first),
            reference(CaudraId::generate()),
        ];
        let texts = refs.iter().map(ToString::to_string).collect();
        (Evidence::new(refs), texts)
    }

    fn file(dir: &TempDir, target: LearningTarget, content: Option<&str>) -> InstructionFile {
        let path = dir.path().join(target.as_str());
        if let Some(content) = content {
            fs::write(&path, content).unwrap();
        }
        InstructionFile::read(target, path).unwrap()
    }

    fn block_report(target: LearningTarget, text: &str, cited: Vec<String>) -> Report {
        let block = Some(Block {
            bullets: vec![Bullet {
                text: text.to_owned(),
                evidence: cited,
            }],
        });
        let mut report = Report::default();
        match target {
            LearningTarget::Project => report.instructions.project = block,
            LearningTarget::Local => report.instructions.local = block,
            _ => report.instructions.global = block,
        }
        report
    }

    #[test_case(LearningTarget::Project, false ; "the_shared_file_wants_two_sessions")]
    #[test_case(LearningTarget::Local, true ; "the_local_file_takes_one")]
    #[test_case(LearningTarget::Global, true ; "the_global_file_takes_one")]
    fn one_session_of_evidence(target: LearningTarget, accepted: bool) {
        let dir = TempDir::new().unwrap();
        let (evidence, refs) = evidence();
        let report = block_report(target, RULE, refs[..2].to_vec());

        let plan = validate(&report, &[file(&dir, target, None)], true, &evidence);

        assert_eq!(plan.blocks[0].bullets.len(), usize::from(accepted));
        if !accepted {
            assert_eq!(plan.dropped[0].reason, ONE_SESSION);
        }
    }

    #[test_case(RULE, vec![], NO_EVIDENCE ; "no_evidence")]
    #[test_case(SECRET_RULE, vec![0, 2], SECRET ; "a_secret")]
    #[test_case("two\nlines", vec![0, 2], MULTI_LINE ; "two_lines")]
    #[test_case("<!-- /caudra:learned -->", vec![0, 2], MARKER ; "a_marker")]
    fn a_bad_bullet_is_dropped(text: &str, cited: Vec<usize>, reason: &str) {
        let dir = TempDir::new().unwrap();
        let (evidence, refs) = evidence();
        let cited = cited.into_iter().map(|index| refs[index].clone()).collect();
        let report = block_report(LearningTarget::Project, text, cited);
        let files = [file(&dir, LearningTarget::Project, None)];

        let plan = validate(&report, &files, true, &evidence);

        assert!(plan.blocks[0].bullets.is_empty());
        assert_eq!(plan.dropped[0].reason, reason);
    }

    #[test]
    fn a_kept_bullet_needs_no_evidence() {
        let dir = TempDir::new().unwrap();
        let existing = file(&dir, LearningTarget::Project, None).rewritten(&[RULE.to_owned()]);
        let files = [file(&dir, LearningTarget::Project, Some(&existing))];
        let report = block_report(LearningTarget::Project, RULE, Vec::new());

        let plan = validate(&report, &files, true, &Evidence::default());

        assert_eq!(plan.blocks[0].bullets, vec![RULE.to_owned()]);
    }

    #[test]
    fn a_conflict_skips_only_its_target() {
        let dir = TempDir::new().unwrap();
        let (evidence, refs) = evidence();
        let files = [
            file(&dir, LearningTarget::Project, Some(HAND_WRITTEN)),
            file(&dir, LearningTarget::Local, None),
        ];
        let mut report = block_report(LearningTarget::Project, RULE, refs.clone());
        report.instructions.local = block_report(LearningTarget::Local, OTHER_RULE, refs)
            .instructions
            .local;
        let plan = validate(&report, &files, true, &evidence);
        fs::write(&files[0].path, OTHER_RULE).unwrap();

        let applied = apply(&plan, &files, None, 1).unwrap();

        assert_eq!(applied.conflicts, vec![LearningTarget::Project]);
        assert_eq!(applied.count(LearningTarget::Local), 1);
        assert_eq!(fs::read_to_string(&files[0].path).unwrap(), OTHER_RULE);
    }

    fn named(dir: &TempDir, target: LearningTarget) -> InstructionFile {
        let path = target_path(target, dir.path(), None).unwrap();
        InstructionFile::read(target, path).unwrap()
    }

    #[test_case(LearningTarget::Local, Some(IGNORE_LINE) ; "a_local_write_ignores_the_file_first")]
    #[test_case(LearningTarget::Project, None ; "a_project_write_leaves_gitignore_alone")]
    fn writing_a_block_to_a_new_file(target: LearningTarget, gitignore: Option<&str>) {
        let dir = TempDir::new().unwrap();
        let (evidence, refs) = evidence();
        let files = [named(&dir, target)];
        let plan = validate(&block_report(target, RULE, refs), &files, true, &evidence);

        let applied = apply(&plan, &files, None, 1).unwrap();

        assert_eq!(applied.count(target), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join(GITIGNORE))
                .ok()
                .as_deref(),
            gitignore
        );
    }

    #[test]
    fn nothing_to_write_creates_no_file() {
        let dir = TempDir::new().unwrap();
        let files = [
            named(&dir, LearningTarget::Project),
            named(&dir, LearningTarget::Local),
        ];
        let mut report = Report::default();
        report.instructions.project = Some(Block::default());
        report.instructions.local = Some(Block::default());
        let plan = validate(&report, &files, true, &Evidence::default());

        let applied = apply(&plan, &files, None, 1).unwrap();

        assert!(applied.edits.is_empty());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_block_edit_reverts_while_it_is_unchanged() {
        let dir = TempDir::new().unwrap();
        let (evidence, refs) = evidence();
        let files = [file(&dir, LearningTarget::Local, Some(HAND_WRITTEN))];
        let plan = validate(
            &block_report(LearningTarget::Local, RULE, refs),
            &files,
            true,
            &evidence,
        );
        let applied = apply(&plan, &files, None, 1).unwrap();
        let origin = EntryOrigin::External;

        let reverted = revert(&applied.edits[0], None, &origin).unwrap();

        assert_eq!(reverted, Reverted::Reverted);
        assert!(bullets(&fs::read_to_string(&files[0].path).unwrap()).is_empty());
        assert_eq!(
            revert(&applied.edits[0], None, &origin).unwrap(),
            Reverted::Changed
        );
    }

    #[test]
    fn a_note_is_written_as_the_runs_and_reverted_away() {
        let dir = TempDir::new().unwrap();
        let state = caudra_storage::StateDir::from_path(dir.path().to_path_buf());
        let memory = MemoryStore::for_cwd(&state, &PathBuf::from("/work/project")).unwrap();
        let (evidence, refs) = evidence();
        let report = Report {
            memory: vec![NoteItem {
                name: NOTE.to_owned(),
                content: NOTE_BODY.to_owned(),
                evidence: refs,
            }],
            ..Report::default()
        };
        let plan = validate(&report, &[], true, &evidence);

        let applied = apply(&plan, &[], Some(&memory), 7).unwrap();

        let entries = memory.journal().current(memory.scope()).unwrap();
        assert_eq!(entries[0].meta.origin, EntryOrigin::Learner(7));
        let reverted = revert(&applied.edits[0], Some(&memory), &EntryOrigin::External);
        assert_eq!(reverted.unwrap(), Reverted::Reverted);
        assert!(note_body(&memory, NOTE).unwrap().is_none());
    }
}
