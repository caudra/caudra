use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use caudra_providers::model::Model;
use caudra_workspace::{ResourceId, ResourceRevision, WorkspacePath};

use crate::command::find_project_ancestor_dirs;
use crate::prompt::profile::SystemPromptProfile;
use crate::remote_project_context::RemoteProjectContext;
use crate::template::Vars;

/// Ranked by precedence, and shared with the remote loader so a project cannot
/// be governed by one file on the host and a different one inside a sandbox.
pub(crate) const INSTRUCTION_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    ".github/copilot-instructions.md",
    "COPILOT.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    "CONVENTIONS.md",
    "GEMINI.md",
    "CODING_AGENT.md",
];

/// Personal, per-project, and gitignored by convention. It never competes with
/// the ranked files; it is layered on top of whichever one a directory supplies.
pub(crate) const LOCAL_INSTRUCTION_FILE: &str = "AGENTS.local.md";
/// The drift diff spans every instruction file at once, so it names the section
/// of the system prompt it patches rather than any one path.
const INSTRUCTIONS_DISPLAY_PATH: &str = "instructions";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InstructionSource {
    Local(PathBuf),
    Remote {
        resource_id: ResourceId,
        revision: ResourceRevision,
    },
}

#[derive(Clone, Default)]
pub struct LoadedInstructions(Arc<Mutex<HashSet<InstructionSource>>>);

impl LoadedInstructions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains_or_insert(&self, path: PathBuf) -> bool {
        self.contains_or_insert_source(InstructionSource::Local(path))
    }

    pub fn contains_or_insert_remote(
        &self,
        resource_id: ResourceId,
        revision: ResourceRevision,
    ) -> bool {
        self.contains_or_insert_source(InstructionSource::Remote {
            resource_id,
            revision,
        })
    }

    fn contains_or_insert_source(&self, source: InstructionSource) -> bool {
        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
        !set.insert(source)
    }
}

#[derive(Default)]
pub struct Instructions {
    pub text: String,
    pub loaded: LoadedInstructions,
}

pub fn is_instruction_file(name: &str) -> bool {
    name == LOCAL_INSTRUCTION_FILE
        || INSTRUCTION_FILES
            .iter()
            .any(|f| *f == name || Path::new(f).file_name().is_some_and(|n| n == name))
}

/// `memory` is the memory view the session froze, see
/// [`MemoryBaseline`](crate::memory::baseline::MemoryBaseline).
pub fn build_system_prompt(
    instructions: &str,
    slots: &crate::prompt::ResolvedSlots,
    tool_filter: &crate::tools::ToolFilter,
    profile: Option<&SystemPromptProfile>,
    memory: Option<&str>,
) -> String {
    crate::prompt::assemble_system(
        &slots.with_native_hints(tool_filter, memory),
        instructions,
        crate::prompt::STANDING_PROMPT,
        profile,
    )
}

/// Announced in the conversation rather than carried by the system prompt, so
/// that a date rollover or a model switch cannot re-cache the conversation.
pub fn environment_block(vars: &Vars, model: &Model) -> String {
    vars.apply(crate::prompt::ENVIRONMENT_PROMPT)
        .replace(crate::prompt::MODEL_SLOT, &model.spec())
}

/// The instruction snapshot the system prompt quotes, pinned.
///
/// Instruction files change while a session runs. Reloading them into the
/// system prompt would invalidate the whole cached prefix on every save, so the
/// baseline stays put and an edit reaches the model as a diff against it.
/// Compaction and conversation revert swap the transcript wholesale and have
/// already paid that cost, which is the one moment a fresh baseline is free.
#[derive(Default)]
pub struct InstructionBaseline {
    instructions: Instructions,
    epoch: u64,
    /// Whether the transcript's last word on the instructions is a diff. A file
    /// that is edited and then put back matches the baseline again, so without
    /// this the diff would stand uncorrected and the model would go on
    /// following a rule that no longer exists.
    drifted: bool,
}

impl InstructionBaseline {
    pub fn adopt(instructions: Instructions, epoch: u64) -> Self {
        Self {
            instructions,
            epoch,
            drifted: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.instructions.text
    }

    pub fn loaded(&self) -> &LoadedInstructions {
        &self.instructions.loaded
    }

    /// `epoch` is the history epoch, which changes only when the conversation
    /// was replaced. Returns the reminder to announce: the current instructions
    /// while they differ from the baseline, a withdrawal on the turn they stop
    /// differing, and `None` once the transcript and disk agree.
    pub fn drift(&mut self, current: Instructions, epoch: u64) -> Option<String> {
        let swapped = epoch != std::mem::replace(&mut self.epoch, epoch);
        if current.text != self.instructions.text {
            if !swapped {
                self.drifted = true;
                return Some(announcement(&self.instructions.text, &current.text));
            }
            self.instructions = current;
        }
        // The system prompt matches disk, either because nothing changed or
        // because the baseline was just adopted. A diff the transcript still
        // shows has to be withdrawn or the model keeps following it.
        std::mem::take(&mut self.drifted)
            .then(|| crate::prompt::INSTRUCTIONS_RESTORED_PROMPT.to_owned())
    }
}

/// How a change to the files reaches a system prompt that cannot be rebuilt.
///
/// A diff is the cheap form and the honest one while the prompt quotes
/// something to patch. A session that started with no instruction files quotes
/// nothing, so the same diff would be the whole text with every line marked
/// added, patching a section that does not exist; those files arrive whole.
fn announcement(baseline: &str, current: &str) -> String {
    if baseline.is_empty() {
        return crate::prompt::INSTRUCTIONS_APPEARED_PROMPT
            .replace(crate::prompt::INSTRUCTIONS_SLOT, current.trim());
    }
    let diff = crate::diff::unified_text(
        baseline,
        current,
        &crate::diff::stat(baseline, current),
        INSTRUCTIONS_DISPLAY_PATH,
    );
    crate::prompt::INSTRUCTIONS_CHANGED_PROMPT.replace(crate::prompt::DIFF_SLOT, &diff)
}

fn read_instruction(path: &Path, loaded: &LoadedInstructions) -> Option<(PathBuf, String)> {
    let canonical = path.canonicalize().ok()?;
    if loaded.contains_or_insert(canonical.clone()) {
        return None;
    }
    let content = fs::read_to_string(&canonical).ok()?;
    Some((canonical, content))
}

/// Where a set of instructions came from. Rendered as an attribute rather than
/// three tag names: the scope is data, and one tag keeps the drift diff and any
/// future parsing simple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionScope {
    Project,
    Local,
    Global,
}

impl InstructionScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Local => "local",
            Self::Global => "global",
        }
    }
}

/// Which filesystem a block came from. Only a sandbox session has two of them,
/// so the attribute is omitted otherwise and a local prompt stays byte-identical
/// to the one the previous release produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionOrigin {
    Host,
    Workspace,
}

impl InstructionOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Workspace => "workspace",
        }
    }
}

struct InstructionFile {
    scope: InstructionScope,
    origin: Option<InstructionOrigin>,
    path: String,
    content: String,
}

/// Delimited so the model can tell a project's instructions from Caudra's own,
/// and so a file that opens with a heading cannot read as a new prompt section.
fn render(files: Vec<InstructionFile>) -> String {
    let mut text = String::new();
    for file in files {
        let origin = file
            .origin
            .map(|origin| format!(" origin=\"{}\"", origin.as_str()))
            .unwrap_or_default();
        text.push_str(&format!(
            "\n\n<instructions scope=\"{}\"{origin} path=\"{}\">\n{}\n</instructions>",
            file.scope.as_str(),
            file.path,
            file.content.trim_end()
        ));
    }
    text
}

fn collect_instruction_files(
    cwd: &str,
    origin: Option<InstructionOrigin>,
    loaded: &LoadedInstructions,
) -> Vec<InstructionFile> {
    let mut out = Vec::new();

    let ancestor_dirs: Vec<_> = find_project_ancestor_dirs(Path::new(cwd)).collect();
    let has_git_root = ancestor_dirs.iter().any(|dir| dir.join(".git").exists());
    let project_dirs = if has_git_root {
        ancestor_dirs
    } else {
        ancestor_dirs.into_iter().take(1).collect()
    };

    // Load root instructions first so cwd instructions come last and override on conflicts.
    for dir in project_dirs.into_iter().rev() {
        for filename in INSTRUCTION_FILES {
            if let Some((canonical, content)) = read_instruction(&dir.join(filename), loaded) {
                out.push(InstructionFile {
                    scope: InstructionScope::Project,
                    origin,
                    path: canonical.display().to_string(),
                    content,
                });
                break;
            }
        }

        if let Some((canonical, content)) =
            read_instruction(&dir.join(LOCAL_INSTRUCTION_FILE), loaded)
        {
            out.push(InstructionFile {
                scope: InstructionScope::Local,
                origin,
                path: canonical.display().to_string(),
                content,
            });
        }
    }

    out
}

/// Always read from the host, in both modes: it is the user's own standing
/// preference, not the project's, and a sandbox has no copy of it.
fn global_instruction_file(
    xdg_config: Option<&Path>,
    origin: Option<InstructionOrigin>,
    loaded: &LoadedInstructions,
) -> Option<InstructionFile> {
    let path = caudra_storage::paths::user_config_dir(xdg_config, "AGENTS.md")?;
    let (canonical, content) = read_instruction(&path, loaded)?;
    Some(InstructionFile {
        scope: InstructionScope::Global,
        origin,
        path: canonical.display().to_string(),
        content,
    })
}

fn local_instruction_files(
    cwd: &str,
    xdg_config: Option<&Path>,
    loaded: &LoadedInstructions,
) -> Vec<InstructionFile> {
    let mut files = collect_instruction_files(cwd, None, loaded);
    files.extend(global_instruction_file(xdg_config, None, loaded));
    files
}

pub fn load_instruction_text(cwd: &str) -> String {
    load_instruction_text_in(cwd, caudra_storage::paths::config_dir().ok().as_deref())
}

pub(crate) fn load_instruction_text_in(cwd: &str, xdg_config: Option<&Path>) -> String {
    let loaded = LoadedInstructions::new();
    render(local_instruction_files(cwd, xdg_config, &loaded))
}

pub fn load_instructions(cwd: &str) -> Instructions {
    load_instructions_in(cwd, caudra_storage::paths::config_dir().ok().as_deref())
}

pub(crate) fn load_instructions_in(cwd: &str, xdg_config: Option<&Path>) -> Instructions {
    let mut instr = Instructions::default();
    instr.text = render(local_instruction_files(cwd, xdg_config, &instr.loaded));
    instr
}

/// A sandbox session reads from two filesystems. `host_cwd` is the directory
/// Caudra itself runs in, which is not the `{cwd}` the model sees: that one
/// names a path inside the VM. Pass `None` where no host directory applies.
pub fn load_remote_instructions(
    context: &RemoteProjectContext,
    host_cwd: Option<&Path>,
) -> Instructions {
    load_remote_instructions_in(
        context,
        host_cwd,
        caudra_storage::paths::config_dir().ok().as_deref(),
    )
}

pub(crate) fn load_remote_instructions_in(
    context: &RemoteProjectContext,
    host_cwd: Option<&Path>,
    xdg_config: Option<&Path>,
) -> Instructions {
    let mut instructions = Instructions::default();
    let mut files = Vec::new();
    for instruction in context.applicable_instructions(&WorkspacePath::root()) {
        if instructions.loaded.contains_or_insert_remote(
            instruction.source.resource_id.clone(),
            instruction.source.revision.clone(),
        ) {
            continue;
        }
        files.push(InstructionFile {
            scope: if instruction.is_personal_overlay() {
                InstructionScope::Local
            } else {
                InstructionScope::Project
            },
            origin: Some(InstructionOrigin::Workspace),
            // The workspace path alone. A resource id and a content digest
            // would add 80 characters per block that tell the model nothing,
            // and the digest changes with the file, so every edit would
            // invalidate the cached prefix twice over.
            path: instruction.source.path.to_string(),
            content: instruction.content.clone(),
        });
    }

    // The host checkout usually seeded the workspace, so most of its files are
    // the same text twice. Matching by path would miss a renamed or relocated
    // copy and would trust a coincidence of naming, so compare the content that
    // actually reaches the model, normalised the way `render` normalises it.
    let mut host = host_cwd.map_or_else(Vec::new, |cwd| {
        collect_instruction_files(
            &cwd.to_string_lossy(),
            Some(InstructionOrigin::Host),
            &instructions.loaded,
        )
    });
    let workspace_rules: HashSet<&str> = files.iter().map(|file| file.content.trim_end()).collect();
    host.retain(|file| !workspace_rules.contains(file.content.trim_end()));
    files.extend(host);

    // Last, as in the local walk: the global file is the weakest claim and the
    // one a project's own files should be read as overriding.
    files.extend(global_instruction_file(
        xdg_config,
        Some(InstructionOrigin::Host),
        &instructions.loaded,
    ));
    instructions.text = render(files);
    if !context.skills().is_empty() {
        instructions.text.push_str("\n\n<available_skills>\n");
        for skill in context.skills() {
            instructions
                .text
                .push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        instructions.text.push_str("</available_skills>");
    }
    instructions
}

pub fn find_remote_nested_instructions(
    context: &RemoteProjectContext,
    path: &WorkspacePath,
    loaded: &LoadedInstructions,
) -> Vec<(String, String)> {
    context
        .applicable_instructions(path)
        .into_iter()
        .filter(|instruction| {
            !loaded.contains_or_insert_remote(
                instruction.source.resource_id.clone(),
                instruction.source.revision.clone(),
            )
        })
        .map(|instruction| {
            (
                instruction.source.path.to_string(),
                instruction.content.clone(),
            )
        })
        .collect()
}

pub fn find_subdirectory_instructions(
    dir: &Path,
    cwd: &Path,
    loaded: &LoadedInstructions,
) -> Vec<(String, String)> {
    let Ok(cwd) = cwd.canonicalize() else {
        return Vec::new();
    };
    let Ok(dir) = dir.canonicalize() else {
        return Vec::new();
    };

    if !dir.starts_with(&cwd) || dir == cwd {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut current = dir.as_path();
    while current != cwd {
        for filename in INSTRUCTION_FILES {
            if let Some((canonical, content)) = read_instruction(&current.join(filename), loaded) {
                results.push((canonical.display().to_string(), content));
                break;
            }
        }
        current = match current.parent() {
            Some(p) => p,
            None => break,
        };
    }
    results
}

#[cfg(test)]
mod tests {
    use std::fs;
    use test_case::test_case;

    use super::*;
    use crate::remote_project_context::tests::AssetService;

    const PLAN_PATH: &str = ".caudra/plans/123.md";
    const BASELINE_TEXT: &str = "# Code guidelines\n";
    const EDITED_TEXT: &str = "# Code guidelines\nbe brief\n";
    const OTHER_TEXT: &str = "# Code guidelines\nbe thorough\n";
    const NO_INSTRUCTIONS: &str = "";
    const DIFF_ADDED_LINE: &str = "+ be brief";
    const EXPECTED_DRIFT_NOTICE: &str = "an edited instruction file should be announced";
    const EXPECTED_APPEARANCE_NOTICE: &str = "a created instruction file should be announced";
    const EPOCH: u64 = 7;
    const WORKSPACE_RULE: &str = "rules the workspace carries";
    const HOST_OVERLAY: &str = "gitignored personal preferences";
    const HOST_DIVERGED: &str = "rules the host alone carries";
    const GLOBAL_RULE: &str = "standing user preferences";
    const ORIGIN_ATTRIBUTE: &str = "origin=";
    const DIGEST_PREFIX: &str = "sha256:";
    const RESOURCE_LABEL: &str = "resource ";
    const ORIGIN_HOST: &str = "origin=\"host\"";
    const ORIGIN_WORKSPACE: &str = "origin=\"workspace\"";

    fn system_prompt() -> String {
        build_system_prompt(
            "",
            &crate::prompt::ResolvedSlots::default(),
            &crate::tools::ToolFilter::All,
            None,
            None,
        )
    }

    /// The system block precedes every message in the cache prefix, so anything
    /// here that varies re-caches the whole conversation when it moves. Mode,
    /// date, and model all vary, so all three are announced instead.
    #[test_case(crate::prompt::PLAN_MODE_MARKER ; "plan_reminder_stays_out")]
    #[test_case(crate::prompt::BUILD_MODE_MARKER ; "build_reminder_stays_out")]
    #[test_case(PLAN_PATH ; "plan_path_stays_out")]
    #[test_case(crate::prompt::ENVIRONMENT_MARKER ; "environment_stays_out")]
    #[test_case("Working directory" ; "cwd_stays_out")]
    fn the_system_prompt_carries_nothing_that_varies(absent: &str) {
        assert!(!system_prompt().contains(absent));
    }

    #[test]
    fn the_environment_block_names_the_model_and_the_date() {
        let vars = Vars::new().set("{cwd}", "/tmp").set("{date}", "2026-09-09");
        let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let block = environment_block(&vars, &model);
        assert!(block.contains(crate::prompt::ENVIRONMENT_MARKER));
        assert!(block.contains(&model.spec()));
        assert!(block.contains("2026-09-09"));
        assert!(!block.contains(crate::prompt::MODEL_SLOT));
    }

    const SCRATCH_SLOT: &str = "{scratch}";
    const SCRATCH_HEADING: &str = "- Scratch directory:";

    fn scratch_block() -> String {
        let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        environment_block(&crate::template::env_vars(), &model)
    }

    /// The model is told where temporary work goes, and told the real path:
    /// the block reads the same variable the shell inherits, so the two cannot
    /// disagree even when the redirect at startup failed.
    #[test]
    fn the_environment_block_names_the_scratch_directory() {
        let _scratch_mode = crate::scratch::ScratchGuard::local();

        let block = scratch_block();

        assert!(block.contains(&std::env::temp_dir().to_string_lossy().into_owned()));
        assert!(!block.contains(SCRATCH_SLOT));
    }

    /// With tools on a remote host the block names the directory created there,
    /// never this machine's temp directory, which nothing the model can call is
    /// able to open.
    #[test]
    fn the_environment_block_names_the_remote_scratch_directory_in_remote_mode() {
        const REMOTE_ROOT: &str = "/var/folders/xy/caudra";
        const REMOTE_PROJECT: &str = "/var/folders/xy/caudra/happy-cute-tick";
        const LOCAL_PATH_STAYS_OUT: &str =
            "a local temp directory must not be offered to remote tools";

        let _scratch_mode =
            crate::scratch::ScratchGuard::remote(Some((REMOTE_ROOT, REMOTE_PROJECT)));

        let block = scratch_block();

        assert!(block.contains(REMOTE_PROJECT));
        assert!(!block.contains(SCRATCH_SLOT));
        assert!(
            !block.contains(&std::env::temp_dir().to_string_lossy().into_owned()),
            "{LOCAL_PATH_STAYS_OUT}"
        );
    }

    /// When the remote host made no directory the block says nothing about one.
    /// Naming a path the model cannot write costs it a wasted call and a prompt;
    /// naming none costs it a sentence.
    #[test]
    fn the_environment_block_omits_the_scratch_directory_when_there_is_none() {
        const NO_HEADING: &str = "a block with no scratch directory must not head a line for one";

        let _scratch_mode = crate::scratch::ScratchGuard::remote(None);

        let block = scratch_block();

        assert!(!block.contains(SCRATCH_HEADING), "{NO_HEADING}");
        assert!(!block.contains(SCRATCH_SLOT));
        assert!(block.contains(crate::prompt::ENVIRONMENT_MARKER));
    }

    #[test]
    fn the_system_prompt_explains_the_mode_protocol() {
        assert!(system_prompt().contains(crate::prompt::MODES_PROMPT.trim_end()));
    }

    /// Reminders arrive as user-role observations, so without this the tag is
    /// the only thing telling the model they are not the user talking.
    #[test]
    fn the_system_prompt_explains_the_reminder_contract() {
        assert!(system_prompt().contains(crate::prompt::REMINDERS_PROMPT.trim_end()));
    }

    #[test]
    fn after_instructions_slot_lands_between_instructions_and_plan() {
        use std::sync::Arc;
        const INSTR: &str = "Project instructions here";
        const EXTRA: &str = "MEMORY_EXTRA";
        let mut slots = crate::prompt::ResolvedSlots::default();
        slots.insert(
            crate::prompt::PromptId::System,
            crate::prompt::Slot::AfterInstructions,
            crate::prompt::SlotEntry {
                plugin: Arc::from("memory"),
                content: EXTRA.into(),
            },
        );
        let prompt = build_system_prompt(
            &format!("\n{INSTR}"),
            &slots,
            &crate::tools::ToolFilter::All,
            None,
            None,
        );
        let positions =
            [INSTR, EXTRA, crate::prompt::MODES_PROMPT.trim()].map(|n| prompt.find(n).unwrap());
        assert!(
            positions.is_sorted(),
            "expected order instructions < slot extra < mode section, got {positions:?}"
        );
    }

    #[test_case("AGENTS.md",                true  ; "direct_match")]
    #[test_case("CLAUDE.md",                true  ; "claude_md")]
    #[test_case("copilot-instructions.md",  true  ; "nested_path_filename")]
    #[test_case(".cursorrules",             true  ; "dotfile")]
    #[test_case("AGENTS.local.md",          true  ; "local_instruction_file")]
    #[test_case("random.md",                false ; "unrelated_file")]
    #[test_case("not-AGENTS.md",            false ; "partial_match")]
    fn is_instruction_file_cases(name: &str, expected: bool) {
        assert_eq!(is_instruction_file(name), expected);
    }

    #[test]
    fn load_instructions_merges_project_and_local() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "team rules").unwrap();
        fs::write(dir.path().join("AGENTS.local.md"), "my preferences").unwrap();

        let text = &load_instructions_in(dir.path().to_str().unwrap(), None).text;
        assert!(text.contains("team rules"));
        assert!(text.contains("my preferences"));
        assert!(
            text.find("team rules").unwrap() < text.find("my preferences").unwrap(),
            "project instructions should come before local instructions"
        );
    }

    /// Unlabelled instructions read as another section of Caudra's own prompt,
    /// and a file that opens with a heading can shadow one. The tags scope them.
    #[test_case(InstructionScope::Project, "AGENTS.md" ; "project")]
    #[test_case(InstructionScope::Local, "AGENTS.local.md" ; "local")]
    fn instruction_files_are_delimited_by_scope_and_path(scope: InstructionScope, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(file), "# Code guidelines\nbe brief\n").unwrap();
        let path = dir.path().join(file).canonicalize().unwrap();

        let text = load_instructions_in(dir.path().to_str().unwrap(), None).text;
        assert_eq!(
            text.trim(),
            format!(
                "<instructions scope=\"{}\" path=\"{}\">\n# Code guidelines\nbe brief\n</instructions>",
                scope.as_str(),
                path.display()
            )
        );
    }

    fn baseline_of(text: &str) -> InstructionBaseline {
        InstructionBaseline::adopt(
            Instructions {
                text: text.into(),
                ..Instructions::default()
            },
            EPOCH,
        )
    }

    fn instructions_of(text: &str) -> Instructions {
        Instructions {
            text: text.into(),
            ..Instructions::default()
        }
    }

    #[test]
    fn an_unchanged_instruction_file_says_nothing() {
        assert!(
            baseline_of(BASELINE_TEXT)
                .drift(instructions_of(BASELINE_TEXT), EPOCH)
                .is_none()
        );
    }

    /// The point of the whole exercise: the system prompt keeps quoting the old
    /// text, so the change has to arrive as a diff against it.
    #[test]
    fn an_edited_instruction_file_is_announced_as_a_diff() {
        let mut baseline = baseline_of(BASELINE_TEXT);
        let notice = baseline
            .drift(instructions_of(EDITED_TEXT), EPOCH)
            .expect(EXPECTED_DRIFT_NOTICE);

        assert!(notice.contains(crate::prompt::INSTRUCTIONS_CHANGED_MARKER));
        assert!(notice.contains(DIFF_ADDED_LINE));
        assert!(!notice.contains(crate::prompt::DIFF_SLOT));
        assert_eq!(
            baseline.text(),
            BASELINE_TEXT,
            "the baseline is what the system prompt still carries"
        );
    }

    /// Replacing the conversation has already cost the prefix cache, so that is
    /// the one moment the system prompt can be rewritten for free.
    #[test]
    fn a_replaced_conversation_adopts_the_new_instructions_silently() {
        let mut baseline = baseline_of(BASELINE_TEXT);
        assert!(
            baseline
                .drift(instructions_of(EDITED_TEXT), EPOCH + 1)
                .is_none()
        );
        assert_eq!(baseline.text(), EDITED_TEXT);
    }

    /// The reminder is deduplicated against the transcript by exact text, so an
    /// unchanging file must not produce a new one on every turn.
    #[test]
    fn a_repeated_drift_notice_is_identical() {
        let mut baseline = baseline_of(BASELINE_TEXT);
        let first = baseline.drift(instructions_of(EDITED_TEXT), EPOCH);
        let second = baseline.drift(instructions_of(EDITED_TEXT), EPOCH);
        assert_eq!(first, second);
        assert!(first.is_some(), "{EXPECTED_DRIFT_NOTICE}");
    }

    /// The gap a "latest wins" rule cannot close: a file put back the way it
    /// was leaves no later block to supersede the diff, so the diff has to be
    /// withdrawn explicitly.
    #[test]
    fn a_reverted_instruction_file_withdraws_the_diff() {
        let mut baseline = baseline_of(BASELINE_TEXT);
        baseline
            .drift(instructions_of(EDITED_TEXT), EPOCH)
            .expect(EXPECTED_DRIFT_NOTICE);

        assert_eq!(
            baseline.drift(instructions_of(BASELINE_TEXT), EPOCH),
            Some(crate::prompt::INSTRUCTIONS_RESTORED_PROMPT.to_owned())
        );
        assert!(
            baseline
                .drift(instructions_of(BASELINE_TEXT), EPOCH)
                .is_none(),
            "the withdrawal is announced once, not on every quiet turn"
        );
    }

    /// Adopting a baseline rewrites the system prompt out from under a diff the
    /// transcript still shows, which leaves it just as stale as a revert does.
    #[test]
    fn an_adopted_baseline_withdraws_an_outstanding_diff() {
        let mut baseline = baseline_of(BASELINE_TEXT);
        baseline
            .drift(instructions_of(EDITED_TEXT), EPOCH)
            .expect(EXPECTED_DRIFT_NOTICE);

        assert_eq!(
            baseline.drift(instructions_of(OTHER_TEXT), EPOCH + 1),
            Some(crate::prompt::INSTRUCTIONS_RESTORED_PROMPT.to_owned())
        );
        assert_eq!(baseline.text(), OTHER_TEXT);
    }

    /// Both bodies have to be the same kind, or the generic supersession rule
    /// has nothing to match the withdrawal against and `standing_notice` keys
    /// them separately.
    #[test]
    fn the_withdrawal_is_the_same_kind_as_the_diff() {
        assert!(
            crate::prompt::INSTRUCTIONS_RESTORED_PROMPT
                .contains(crate::prompt::INSTRUCTIONS_CHANGED_MARKER)
        );
    }

    /// A swap that lands while the files are untouched must not leave the
    /// baseline looking stale, or the next real edit rewrites the system prompt
    /// against a cache that has since gone warm again.
    #[test]
    fn a_replacement_without_an_edit_still_settles_the_baseline() {
        let mut baseline = baseline_of(BASELINE_TEXT);
        assert!(
            baseline
                .drift(instructions_of(BASELINE_TEXT), EPOCH + 1)
                .is_none()
        );
        assert!(
            baseline
                .drift(instructions_of(EDITED_TEXT), EPOCH + 1)
                .is_some(),
            "{EXPECTED_DRIFT_NOTICE}"
        );
        assert_eq!(baseline.text(), BASELINE_TEXT);
    }

    /// A session that began with no instruction files has nothing in its system
    /// prompt for a diff to patch, so the file that appears has to arrive as
    /// itself rather than as the same text with every line marked added.
    #[test]
    fn a_created_instruction_file_arrives_whole() {
        let mut baseline = baseline_of(NO_INSTRUCTIONS);
        let notice = baseline
            .drift(instructions_of(EDITED_TEXT), EPOCH)
            .expect(EXPECTED_APPEARANCE_NOTICE);

        assert!(notice.contains(crate::prompt::INSTRUCTIONS_CHANGED_MARKER));
        assert!(notice.contains(EDITED_TEXT.trim()));
        assert!(!notice.contains(crate::prompt::INSTRUCTIONS_SLOT));
        assert!(!notice.contains(DIFF_ADDED_LINE));
        assert!(!notice.contains(&format!("--- {INSTRUCTIONS_DISPLAY_PATH}")));
        assert_eq!(
            baseline.text(),
            NO_INSTRUCTIONS,
            "adopting the text would splice a section into the cached system prompt"
        );
    }

    /// The baseline stays empty after an appearance, so there is still nothing
    /// to diff against and every later edit is another whole copy.
    #[test]
    fn an_edit_after_an_appearance_resends_the_whole_text() {
        let mut baseline = baseline_of(NO_INSTRUCTIONS);
        baseline
            .drift(instructions_of(BASELINE_TEXT), EPOCH)
            .expect(EXPECTED_APPEARANCE_NOTICE);
        let notice = baseline
            .drift(instructions_of(EDITED_TEXT), EPOCH)
            .expect(EXPECTED_APPEARANCE_NOTICE);

        assert!(notice.contains(EDITED_TEXT.trim()));
        assert!(!notice.contains(DIFF_ADDED_LINE));
    }

    /// Deleting the file again leaves the announcement standing over rules that
    /// no longer exist, the same gap a reverted edit leaves.
    #[test]
    fn a_deleted_instruction_file_withdraws_the_announcement() {
        let mut baseline = baseline_of(NO_INSTRUCTIONS);
        baseline
            .drift(instructions_of(EDITED_TEXT), EPOCH)
            .expect(EXPECTED_APPEARANCE_NOTICE);

        assert_eq!(
            baseline.drift(instructions_of(NO_INSTRUCTIONS), EPOCH),
            Some(crate::prompt::INSTRUCTIONS_RESTORED_PROMPT.to_owned())
        );
        assert!(
            baseline
                .drift(instructions_of(NO_INSTRUCTIONS), EPOCH)
                .is_none(),
            "the withdrawal is announced once, not on every quiet turn"
        );
    }

    #[test]
    fn load_instructions_local_without_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.local.md"), "solo preferences").unwrap();
        assert!(
            load_instructions_in(dir.path().to_str().unwrap(), None)
                .text
                .contains("solo preferences")
        );
    }

    #[test]
    fn load_instructions_empty_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_instructions_in(dir.path().to_str().unwrap(), None)
                .text
                .is_empty()
        );
    }

    #[test]
    fn load_instructions_empty_when_config_dir_has_no_global_file() {
        let cwd = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        assert!(
            load_instructions_in(cwd.path().to_str().unwrap(), Some(config.path()))
                .text
                .is_empty()
        );
    }

    #[test]
    fn load_instructions_includes_the_global_config_file() {
        let cwd = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        fs::write(config.path().join("AGENTS.md"), "global rules").unwrap();

        let text = load_instructions_in(cwd.path().to_str().unwrap(), Some(config.path())).text;
        assert!(text.contains("global rules"));
    }

    #[test]
    fn load_instructions_includes_parent_directory_instructions() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();

        let sub = dir.path().join("crates").join("a");
        fs::create_dir_all(&sub).unwrap();

        fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
        fs::write(sub.join("AGENTS.md"), "crate rules").unwrap();

        let text = load_instructions_in(sub.to_str().unwrap(), None).text;
        assert!(
            text.contains("crate rules"),
            "should load instructions from cwd"
        );
        assert!(
            text.contains("root rules"),
            "should load instructions from project root"
        );
        assert!(
            text.find("root rules").unwrap() < text.find("crate rules").unwrap(),
            "root instructions should come before closer instructions"
        );
    }

    #[test]
    fn find_subdirectory_instructions_discovers_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src").join("api");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("src").join("AGENTS.md"), "api rules").unwrap();

        let loaded = LoadedInstructions::new();
        let results = find_subdirectory_instructions(&sub, dir.path(), &loaded);

        assert_eq!(results.len(), 1);
        assert!(results[0].0.ends_with("AGENTS.md"));
        assert_eq!(results[0].1, "api rules");
    }

    #[test]
    fn find_subdirectory_instructions_skips_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();

        let loaded = LoadedInstructions::new();
        let from_root = find_subdirectory_instructions(dir.path(), dir.path(), &loaded);
        assert!(from_root.is_empty(), "should skip root-level directory");
    }

    #[test]
    fn find_subdirectory_instructions_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        let agents_path = sub.join("AGENTS.md");
        fs::write(&agents_path, "rules").unwrap();

        let canonical = agents_path.canonicalize().unwrap();
        let loaded = LoadedInstructions::new();
        loaded.contains_or_insert(canonical);
        let pre_loaded = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        assert!(pre_loaded.is_empty(), "should skip already-loaded files");

        let loaded = LoadedInstructions::new();
        let first = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        let second = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        assert_eq!(first.len(), 1);
        assert!(
            second.is_empty(),
            "should not return same file twice across calls"
        );
    }

    /// A sandbox session reads from two filesystems. The workspace supplies the
    /// project's own rules; the host supplies what a transfer respecting
    /// gitignore could never have carried, plus the user's global file.
    #[test]
    fn a_remote_session_layers_the_host_overlay_and_global_over_the_workspace() {
        let host = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        fs::write(host.path().join("AGENTS.md"), WORKSPACE_RULE).unwrap();
        fs::write(host.path().join("AGENTS.local.md"), HOST_OVERLAY).unwrap();
        fs::write(config.path().join("AGENTS.md"), GLOBAL_RULE).unwrap();
        let context = AssetService::instruction_fixture(&[("AGENTS.md", WORKSPACE_RULE)]);

        let text =
            load_remote_instructions_in(&context, Some(host.path()), Some(config.path())).text;

        // The host `AGENTS.md` is the same text the workspace already supplied,
        // so it is dropped rather than said twice.
        assert_eq!(text.matches(WORKSPACE_RULE).count(), 1, "{text}");
        assert!(text.contains(HOST_OVERLAY), "{text}");
        assert!(
            text.find(WORKSPACE_RULE).unwrap() < text.find(HOST_OVERLAY).unwrap(),
            "{text}"
        );
        assert!(
            text.find(HOST_OVERLAY).unwrap() < text.find(GLOBAL_RULE).unwrap(),
            "the global file is the weakest claim and renders last: {text}"
        );
    }

    /// Matching by path would miss a relocated copy and would trust a
    /// coincidence of naming, so the comparison is the text itself.
    #[test]
    fn a_host_file_survives_only_when_its_content_differs_from_the_workspace() {
        let host = tempfile::tempdir().unwrap();
        fs::write(host.path().join("AGENTS.md"), HOST_DIVERGED).unwrap();
        let context = AssetService::instruction_fixture(&[("AGENTS.md", WORKSPACE_RULE)]);

        let text = load_remote_instructions_in(&context, Some(host.path()), None).text;

        assert!(text.contains(WORKSPACE_RULE), "{text}");
        assert!(text.contains(HOST_DIVERGED), "{text}");
    }

    /// Trailing whitespace never reaches the model, because `render` trims it,
    /// so it must not be what makes a duplicate look like a difference.
    #[test]
    fn trailing_whitespace_alone_does_not_make_a_host_file_differ() {
        let host = tempfile::tempdir().unwrap();
        fs::write(
            host.path().join("AGENTS.md"),
            format!("{WORKSPACE_RULE}\n\n"),
        )
        .unwrap();
        let context = AssetService::instruction_fixture(&[("AGENTS.md", WORKSPACE_RULE)]);

        let text = load_remote_instructions_in(&context, Some(host.path()), None).text;

        assert_eq!(text.matches(WORKSPACE_RULE).count(), 1, "{text}");
    }

    /// The attribute is what lets the model tell a rule about the sandbox from
    /// one about the machine it is driving the sandbox from.
    #[test]
    fn origin_marks_remote_blocks_and_never_appears_in_a_local_render() {
        let host = tempfile::tempdir().unwrap();
        fs::write(host.path().join("AGENTS.md"), HOST_DIVERGED).unwrap();
        let context = AssetService::instruction_fixture(&[("AGENTS.md", WORKSPACE_RULE)]);

        let remote = load_remote_instructions_in(&context, Some(host.path()), None).text;
        assert!(remote.contains(ORIGIN_WORKSPACE), "{remote}");
        assert!(remote.contains(ORIGIN_HOST), "{remote}");

        // Local prompts stay byte-identical to the previous release's, so an
        // upgrade does not invalidate every cached prefix.
        let local = load_instructions_in(host.path().to_str().unwrap(), None).text;
        assert!(!local.contains(ORIGIN_ATTRIBUTE), "{local}");
    }

    /// Workspace identity is bookkeeping. Naming it in the prompt would cost
    /// tokens on every request and, because the digest tracks the content,
    /// would change the system prompt on every edit of the file it names.
    #[test]
    fn workspace_identity_never_reaches_the_prompt() {
        let context = AssetService::instruction_fixture(&[("AGENTS.md", WORKSPACE_RULE)]);

        let text = load_remote_instructions_in(&context, None, None).text;
        let nested = find_remote_nested_instructions(
            &context,
            &WorkspacePath::new("src/lib.rs").unwrap(),
            &LoadedInstructions::new(),
        );

        assert!(text.contains("path=\"AGENTS.md\""), "{text}");
        assert!(!text.contains(DIGEST_PREFIX), "{text}");
        assert!(!text.contains(RESOURCE_LABEL), "{text}");
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].0, "AGENTS.md");
    }

    #[test]
    fn load_instructions_populates_loaded_set() {
        let dir = tempfile::tempdir().unwrap();
        let agents_path = dir.path().join("AGENTS.md");
        fs::write(&agents_path, "content").unwrap();

        let instr = load_instructions_in(dir.path().to_str().unwrap(), None);
        assert!(
            instr
                .loaded
                .contains_or_insert(agents_path.canonicalize().unwrap())
        );
    }
}
