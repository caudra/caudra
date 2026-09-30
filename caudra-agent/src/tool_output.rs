use std::sync::{Arc, LazyLock};
use std::thread;

use caudra_storage::StateDir;
use caudra_storage::sessions::persisted_session_ids;
use caudra_storage::tool_outputs::{ToolOutputRef, ToolOutputStore, line_count};
use tracing::warn;

use caudra_config::{MIN_PER_TOOL_OUTPUT_BYTES, MIN_PER_TOOL_OUTPUT_LINES};

use crate::tools::ToolContext;
use crate::{IndexOutput, TextOutput, ToolDoneEvent, ToolOutput, ToolOutputLimits};

const PERSIST_THRESHOLD_BYTES: usize = 8 * 1024;
const READ_LIMIT: usize = 200;
const CLEANUP_THREAD_NAME: &str = "tool-output-reclaim";
const MAX_COMMAND_LABEL_BYTES: usize = 4096;
const SHELL_LABEL: &str = "shell";
const OUTPUT_LABEL: &str = "output";
const SHELL_LABEL_COMMANDS: &[(&str, &[&str])] = &[
    (
        "cargo",
        &[
            "build", "check", "test", "nextest", "clippy", "fmt", "run", "doc",
        ],
    ),
    (
        "git",
        &[
            "status", "diff", "log", "show", "fetch", "pull", "push", "add", "commit",
        ],
    ),
    ("npm", &["test", "run", "install", "ci", "build"]),
    ("pnpm", &["test", "run", "install", "build", "lint"]),
    ("yarn", &["test", "run", "install", "build", "lint"]),
    ("bun", &["test", "run", "install", "build"]),
    ("go", &["build", "test", "run", "vet", "fmt"]),
    ("just", &["check", "test", "lint", "build", "fmt"]),
    ("make", &[]),
    ("cmake", &[]),
    ("ninja", &[]),
    ("pytest", &[]),
    ("rustc", &[]),
];

static DEFAULT_STORE: LazyLock<Option<Arc<ToolOutputStore>>> = LazyLock::new(|| {
    StateDir::resolve()
        .map(|state_dir| {
            let store = Arc::new(ToolOutputStore::new(state_dir.clone()));
            // Reclaiming orphans enumerates every persisted session and walks
            // the whole tool-output tree. Nothing the store serves depends on
            // it, but running it here spent that on whichever thread first
            // touched the store -- at startup, the one that has not drawn a
            // frame yet. Detached, so a slow filesystem delays only the
            // reclaim.
            let sweeper = Arc::clone(&store);
            if let Err(error) = thread::Builder::new()
                .name(CLEANUP_THREAD_NAME.to_owned())
                .spawn(move || reclaim_orphans(&sweeper, &state_dir))
            {
                warn!(%error, "tool output cleanup not started");
            }
            store
        })
        .map_err(|error| warn!(%error, "tool output store unavailable"))
        .ok()
});

fn reclaim_orphans(store: &ToolOutputStore, state_dir: &StateDir) {
    match persisted_session_ids(state_dir) {
        Ok(session_ids) => {
            if let Err(error) = store.cleanup_orphans(&session_ids) {
                warn!(%error, "failed to clean up orphaned tool outputs");
            }
        }
        Err(error) => warn!(%error, "failed to enumerate sessions for tool output cleanup"),
    }
}

pub(crate) fn default_store() -> Option<Arc<ToolOutputStore>> {
    DEFAULT_STORE.clone()
}

pub(crate) async fn limit(done: &mut ToolDoneEvent, ctx: &ToolContext) {
    limit_named(done, ctx, None).await;
}

pub(crate) fn shell_output_label(command: &str) -> String {
    if command.len() > MAX_COMMAND_LABEL_BYTES
        || !command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" \t-_.:/=,@%+".contains(&byte))
    {
        return SHELL_LABEL.into();
    }
    let mut words = command.split_ascii_whitespace();
    let Some((program, subcommands)) = words.next().and_then(|program| {
        SHELL_LABEL_COMMANDS
            .iter()
            .find(|(name, _)| *name == program)
    }) else {
        return SHELL_LABEL.into();
    };
    match words.next().filter(|word| subcommands.contains(word)) {
        Some(subcommand) => format!("{program}-{subcommand}"),
        None => (*program).into(),
    }
}

pub(crate) async fn limit_named(
    done: &mut ToolDoneEvent,
    ctx: &ToolContext,
    producer: Option<&str>,
) {
    let limits = effective_limits(done.output_limits, &ctx.config);
    let pre_persisted = done.model_output_from_ref;
    let pre_persisted_load = if pre_persisted {
        done.model_output_from_ref = false;
        load_pre_persisted(done, ctx, limits).await
    } else {
        PrePersistedLoad::None
    };
    let full_model_output = done.composed_model_output();
    let oversized = matches!(pre_persisted_load, PrePersistedLoad::Preview)
        || exceeds_limits(&full_model_output, limits.max_lines, limits.max_bytes);
    if !oversized
        && (pre_persisted || done.is_error || full_model_output.len() <= PERSIST_THRESHOLD_BYTES)
    {
        return;
    }

    let producer = producer.unwrap_or(&done.tool);
    let label = if producer.is_empty() || producer == "unknown" {
        OUTPUT_LABEL.to_owned()
    } else {
        format!("{OUTPUT_LABEL}-{producer}")
    };
    let output_ref = if pre_persisted && done.output.instructions().is_some() {
        let output_ref = persist(ctx, full_model_output, label).await;
        if output_ref.is_some() {
            done.output_ref = output_ref.clone();
        }
        output_ref
    } else if pre_persisted {
        done.output_ref.clone()
    } else {
        let output_ref = persist(ctx, full_model_output, label).await;
        done.output_ref = output_ref.clone();
        output_ref
    };
    if !oversized {
        return;
    }

    let marker = marker(
        output_ref
            .as_ref()
            .filter(|_| !pre_persisted || pre_persisted_load.loaded()),
    );
    let suffix = done
        .model_suffix()
        .map(|suffix| suffix.trim_matches(['\r', '\n']))
        .filter(|suffix| !suffix.is_empty());
    let mut source = done
        .model_output
        .clone()
        .unwrap_or_else(|| done.output.as_text());
    if suffix.is_some() {
        source.truncate(source.trim_end_matches(['\r', '\n']).len());
    }
    let trailer = suffix.map_or_else(String::new, |suffix| format!("\n\n{suffix}"));
    let body = preview_body(
        &source,
        &marker,
        &trailer,
        limits.max_lines,
        limits.max_bytes,
    );
    let composed = format!("{body}{trailer}");
    done.model_output = Some(if fits(&composed, limits.max_lines, limits.max_bytes) {
        body
    } else {
        done.model_suffix = None;
        hard_bound(&composed, limits.max_lines, limits.max_bytes)
    });
    bound_presentation(
        &mut done.output,
        &marker,
        limits.max_lines,
        limits.max_bytes,
        matches!(pre_persisted_load, PrePersistedLoad::Preview),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrePersistedLoad {
    None,
    Full,
    Preview,
    Failed,
}

impl PrePersistedLoad {
    fn loaded(self) -> bool {
        matches!(self, Self::Full | Self::Preview)
    }
}

async fn load_pre_persisted(
    done: &mut ToolDoneEvent,
    ctx: &ToolContext,
    limits: ToolOutputLimits,
) -> PrePersistedLoad {
    let has_instructions = done.output.instructions().is_some();
    let result = match (&done.output_ref, &ctx.session_id, &ctx.tool_output_store) {
        (Some(output_ref), Some(session_id), Some(store)) => {
            let id = output_ref.id.clone();
            let preview = !has_instructions
                && (output_ref.line_count > limits.max_lines
                    || output_ref.byte_count > limits.max_bytes);
            let storage_session_id = session_id.id();
            let store = Arc::clone(store);
            smol::unblock(move || {
                if preview {
                    store
                        .preview(storage_session_id, id, limits.max_lines, limits.max_bytes)
                        .map(|preview| preview.into_text())
                } else {
                    store.load_text(storage_session_id, id)
                }
            })
            .await
            .map(|output| (output, preview))
            .map_err(|error| error.to_string())
        }
        (None, _, _) => Err("streamed output reference is missing".into()),
        (_, None, _) => Err("current session is unavailable".into()),
        (_, _, None) => Err("tool output store is unavailable".into()),
    };

    match result {
        Ok((output, preview)) => {
            if let Some(text) = text_output_mut(&mut done.output) {
                text.text = output;
                if preview {
                    PrePersistedLoad::Preview
                } else {
                    PrePersistedLoad::Full
                }
            } else {
                set_load_error(done, "streamed output is not plain or markdown text");
                PrePersistedLoad::Failed
            }
        }
        Err(error) => {
            set_load_error(done, &error);
            PrePersistedLoad::Failed
        }
    }
}

fn set_load_error(done: &mut ToolDoneEvent, error: &str) {
    let id = done
        .output_ref
        .as_ref()
        .map_or_else(|| "unknown".into(), |output_ref| output_ref.id.to_string());
    let message = format!("Failed to load streamed tool output {id}: {error}");
    if let Some(text) = text_output_mut(&mut done.output) {
        text.text = message;
    } else {
        done.output = ToolOutput::Plain(message.into());
    }
    done.is_error = true;
    done.output_ref = None;
}

fn effective_limits(
    output_limits: Option<ToolOutputLimits>,
    config: &caudra_config::AgentConfig,
) -> ToolOutputLimits {
    let fallback = ToolOutputLimits {
        max_lines: config.max_output_lines.max(1),
        max_bytes: config.max_output_bytes.max(1),
    };
    let Some(output_limits) = output_limits else {
        return fallback;
    };
    ToolOutputLimits {
        max_lines: if output_limits.max_lines > 0 {
            output_limits.max_lines.max(MIN_PER_TOOL_OUTPUT_LINES)
        } else {
            fallback.max_lines
        },
        max_bytes: if output_limits.max_bytes > 0 {
            output_limits.max_bytes.max(MIN_PER_TOOL_OUTPUT_BYTES)
        } else {
            fallback.max_bytes
        },
    }
}

async fn persist(ctx: &ToolContext, text: String, label: String) -> Option<ToolOutputRef> {
    let (Some(session_id), Some(store)) = (&ctx.session_id, &ctx.tool_output_store) else {
        return None;
    };
    let session_id = session_id.clone();
    let storage_session_id = session_id.id();
    let store = Arc::clone(store);
    match smol::unblock(move || store.put_named(storage_session_id, &text, &label)).await {
        Ok(output_ref) => Some(output_ref),
        Err(error) => {
            warn!(%error, session_id = %session_id, "failed to persist tool output");
            None
        }
    }
}

fn marker(output_ref: Option<&ToolOutputRef>) -> String {
    match output_ref {
        Some(output_ref) => format!(
            "[Tool output truncated. Full output ID: {id}. Use {tool}(output_id=\"{id}\", offset=1, limit={READ_LIMIT}), optionally with pattern=\"...\" to search it.]",
            id = output_ref.id,
            tool = crate::tools::TOOL_OUTPUT_TOOL_NAME,
        ),
        None => "[Tool output truncated. Full output was unavailable; rerun this tool with narrower output.]".into(),
    }
}

/// Bounds a presentation in place; it never replaces one. Only a variant whose
/// text is the model's copy rather than the card's source can be cut here:
/// `Index` keeps its semantic lines, `Batch` its roster, and a variant drawn
/// entirely from structured fields is left whole. The reader is already held to
/// `ui.tool_output_lines` with an expand control and the model reads the
/// bounded `model_output`, so collapsing the variant to `Plain` bought no bytes
/// either of them was spending and cost the card the shape it is drawn from.
fn bound_presentation(
    output: &mut ToolOutput,
    marker: &str,
    max_lines: usize,
    max_bytes: usize,
    force_marker: bool,
) {
    let rendered = output.as_text();
    if !force_marker && fits(&rendered, max_lines, max_bytes) {
        return;
    }

    match output {
        ToolOutput::Index(IndexOutput::File { skeleton, .. }) => {
            *skeleton = preview_body(&rendered, marker, "", max_lines, max_bytes);
        }
        ToolOutput::Index(IndexOutput::Directory { listing, .. }) => {
            *listing = preview_body(&rendered, marker, "", max_lines, max_bytes);
        }
        ToolOutput::Batch { text, .. } => {
            *text = preview_body(&rendered, marker, "", max_lines, max_bytes);
        }
        ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text) => {
            let source = text.text.clone();
            let trailer = rendered.strip_prefix(&source).unwrap_or_default();
            let ending = format!("{marker}{trailer}");
            if fits(&ending, max_lines, max_bytes) {
                text.text = preview_body(&source, marker, trailer, max_lines, max_bytes);
            } else {
                text.text = preview_body(&rendered, marker, "", max_lines, max_bytes);
                text.instructions = None;
            }
        }
        _ => {}
    }
}

fn text_output_mut(output: &mut ToolOutput) -> Option<&mut TextOutput> {
    match output {
        ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text) => {
            Some(text)
        }
        _ => None,
    }
}

fn preview_body(
    source: &str,
    marker: &str,
    trailer: &str,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    let ending = format!("{marker}{trailer}");
    if !fits(&ending, max_lines, max_bytes) {
        return hard_bound(marker, max_lines, max_bytes);
    }

    let source = source.trim_matches(['\r', '\n']);
    let source_line_budget = max_lines.saturating_sub(line_count(&ending).saturating_add(2));
    let source_byte_budget = max_bytes.saturating_sub(ending.len().saturating_add(7));
    if source.is_empty() || source_line_budget < 2 || source_byte_budget < 2 {
        return marker.to_owned();
    }

    let head_line_budget = source_line_budget.div_ceil(2);
    let tail_line_budget = source_line_budget / 2;
    let head_byte_budget = source_byte_budget.div_ceil(2);
    let tail_byte_budget = source_byte_budget / 2;
    let head = head(source, head_line_budget, head_byte_budget);
    let tail = tail(source, tail_line_budget, tail_byte_budget);
    if head.is_empty() || tail.is_empty() {
        return marker.to_owned();
    }

    let preview = format!("{head}\n...\n{tail}\n\n{marker}");
    if fits(&(preview.clone() + trailer), max_lines, max_bytes) {
        preview
    } else {
        marker.to_owned()
    }
}

fn head(source: &str, max_lines: usize, max_bytes: usize) -> &str {
    let line_end = source
        .match_indices('\n')
        .nth(max_lines.saturating_sub(1))
        .map_or(source.len(), |(index, _)| index);
    let end = source[..line_end].floor_char_boundary(max_bytes.min(line_end));
    source[..end].trim_end_matches(['\r', '\n'])
}

fn tail(source: &str, max_lines: usize, max_bytes: usize) -> &str {
    let line_start = source
        .rmatch_indices('\n')
        .nth(max_lines.saturating_sub(1))
        .map_or(0, |(index, _)| index + 1);
    let available = &source[line_start..];
    let mut start = available.len().saturating_sub(max_bytes);
    while !available.is_char_boundary(start) {
        start += 1;
    }
    available[start..].trim_matches(['\r', '\n'])
}

fn hard_bound(text: &str, max_lines: usize, max_bytes: usize) -> String {
    if fits(text, max_lines, max_bytes) {
        return text.to_owned();
    }
    if max_lines == 0 || max_bytes == 0 {
        return String::new();
    }

    let mut end = 0;
    let mut lines = 1;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if next > max_bytes || (character == '\n' && lines >= max_lines) {
            break;
        }
        end = next;
        if character == '\n' {
            lines += 1;
        }
    }
    text[..end].trim_end_matches(['\r', '\n']).to_owned()
}

fn exceeds_limits(text: &str, max_lines: usize, max_bytes: usize) -> bool {
    !fits(text, max_lines, max_bytes)
}

fn fits(text: &str, max_lines: usize, max_bytes: usize) -> bool {
    text.len() <= max_bytes && line_count(text) <= max_lines
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use caudra_storage::tool_outputs::ToolOutputStore;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::InstructionBlock;
    use crate::tools::ToolEffect;
    use crate::{
        BatchToolEntry, BatchToolStatus, GrepFileEntry, GrepLine, GrepMatchGroup, PatchedFile,
    };
    use std::mem::discriminant;

    const LARGE_BYTE_LIMIT: usize = 100_000;
    const ROSTER_LOST: &str = "an oversized batch must keep the roster its card is drawn from";
    const SHAPE_LOST: &str = "an oversized result must keep the variant its card is drawn from";

    #[test_case("cargo test private_test_name --token=private", "cargo-test"; "ignores_arguments")]
    #[test_case("git status --short", "git-status"; "known_subcommand")]
    #[test_case("npm run private_script", "npm-run"; "ignores_script_name")]
    #[test_case("cargo private_command", "cargo"; "unknown_subcommand")]
    #[test_case("pytest private/path.py", "pytest"; "ignores_path")]
    #[test_case("TOKEN=private cargo test", SHELL_LABEL; "environment")]
    #[test_case("/private/bin/cargo test", SHELL_LABEL; "executable_path")]
    #[test_case("private_program", SHELL_LABEL; "unknown_program")]
    #[test_case("sudo cargo test", SHELL_LABEL; "wrapper")]
    #[test_case("cargo test | cat", SHELL_LABEL; "pipeline")]
    #[test_case("cargo test && cargo check", SHELL_LABEL; "compound")]
    #[test_case("cargo test > private", SHELL_LABEL; "redirect")]
    #[test_case("cargo test $(private)", SHELL_LABEL; "substitution")]
    #[test_case("cargo test `private`", SHELL_LABEL; "backticks")]
    #[test_case("cargo test 'private'", SHELL_LABEL; "quoted")]
    #[test_case("cargo test\ncargo check", SHELL_LABEL; "multiline")]
    #[test_case("cargo test # private", SHELL_LABEL; "comment")]
    #[test_case("", SHELL_LABEL; "empty")]
    fn shell_labels_use_only_fixed_command_names(command: &str, expected: &str) {
        assert_eq!(shell_output_label(command), expected);
    }

    #[test]
    fn shell_labels_bound_command_inspection() {
        let command = format!("cargo test {}", "x".repeat(MAX_COMMAND_LABEL_BYTES));
        assert_eq!(shell_output_label(&command), SHELL_LABEL);
    }

    #[test_case("file_grep", None, "output-file-grep"; "tool_name")]
    #[test_case("shell", Some("cargo-test"), "output-cargo-test"; "shell_label")]
    #[test_case("shell", Some(SHELL_LABEL), "output-shell"; "shell_fallback")]
    #[test_case("unknown", None, OUTPUT_LABEL; "generic_fallback")]
    fn persisted_outputs_use_producer_labels(tool: &str, producer: Option<&str>, expected: &str) {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 8, 360);
        let text = "private output content\n".repeat(100);
        for suffix in [String::new(), "-2".into()] {
            let mut event = done(text.clone(), false);
            event.tool = Arc::from(tool);
            smol::block_on(limit_named(&mut event, &ctx, producer));
            let reference = event.output_ref.unwrap();
            assert_eq!(reference.id.as_str(), format!("{expected}{suffix}"));
            assert_eq!(store.load_text(session.id(), reference.id).unwrap(), text);
        }
    }

    #[test]
    fn producer_label_does_not_rename_a_pre_persisted_output() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 8, 360);
        let text = "streamed output\n".repeat(100);
        let reference = store
            .put_named(session.id(), &text, "existing-label")
            .unwrap();
        let mut event = done(String::new(), false);
        event.output_ref = Some(reference.clone());
        event.model_output_from_ref = true;
        smol::block_on(limit_named(&mut event, &ctx, Some("cargo-test")));
        assert_eq!(event.output_ref.unwrap().id, reference.id);
    }

    #[test_case("", 0 ; "empty")]
    #[test_case("x", 1 ; "unterminated")]
    #[test_case("x\n", 1 ; "trailing_lf")]
    #[test_case("x\r\n", 1 ; "trailing_crlf")]
    #[test_case("x\n\n", 2 ; "consecutive_blank_lines")]
    fn central_limit_uses_managed_line_count(text: &str, expected: usize) {
        assert_eq!(line_count(text), expected);
        assert!(fits(text, expected.max(1), text.len().max(1)));
    }

    fn done(text: String, is_error: bool) -> ToolDoneEvent {
        ToolDoneEvent {
            id: "tool-1".into(),
            tool: Arc::from("test"),
            output: ToolOutput::Plain(text.into()),
            is_error,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: crate::ToolAccounting::default(),
        }
    }

    fn context(max_lines: usize, max_bytes: usize) -> ToolContext {
        let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
        ctx.config.max_output_lines = max_lines;
        ctx.config.max_output_bytes = max_bytes;
        ctx
    }

    fn stored_context(
        temp: &TempDir,
        max_lines: usize,
        max_bytes: usize,
    ) -> (ToolContext, Arc<ToolOutputStore>, SessionRef) {
        let store = Arc::new(ToolOutputStore::new(StateDir::from_path(
            temp.path().to_path_buf(),
        )));
        let session = SessionRef::generate();
        let mut ctx = context(max_lines, max_bytes);
        ctx.session_id = Some(session.clone());
        ctx.tool_output_store = Some(Arc::clone(&store));
        (ctx, store, session)
    }

    #[test]
    fn line_limit_keeps_head_and_tail() {
        let text = (0..30)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut done = done(text, false);
        let ctx = context(7, LARGE_BYTE_LIMIT);

        smol::block_on(limit(&mut done, &ctx));

        let model_output = done.model_output.unwrap();
        assert!(line_count(&model_output) <= ctx.config.max_output_lines);
        assert!(model_output.starts_with("line-0"));
        assert!(model_output.contains("line-29"));
        assert!(model_output.contains("Full output was unavailable"));
    }

    #[test]
    fn byte_limit_keeps_head_and_tail() {
        let text = format!("HEAD{}TAIL", "x".repeat(1_000));
        let mut done = done(text, false);
        let ctx = context(100, 240);

        smol::block_on(limit(&mut done, &ctx));

        let model_output = done.model_output.unwrap();
        assert!(model_output.len() <= ctx.config.max_output_bytes);
        assert!(model_output.starts_with("HEAD"));
        assert!(model_output.contains("TAIL"));
    }

    #[test]
    fn byte_preview_is_utf8_safe() {
        let text = format!("start-{}-end", "蟹".repeat(400));
        let mut done = done(text, false);
        let ctx = context(100, 241);

        smol::block_on(limit(&mut done, &ctx));

        let model_output = done.model_output.unwrap();
        assert!(model_output.len() <= ctx.config.max_output_bytes);
        assert!(model_output.starts_with("start-"));
        assert!(model_output.contains("-end"));
    }

    #[test]
    fn limits_and_persists_exact_model_output_instead_of_presentation() {
        let model = format!("MODEL-{}-TAIL", "x".repeat(1_000));
        let mut event = done("short presentation".into(), false);
        event.model_output = Some(model);
        let ctx = context(100, 240);

        smol::block_on(limit(&mut event, &ctx));

        let bounded = event.model_output.expect("bounded model output");
        assert!(bounded.starts_with("MODEL-"));
        assert!(bounded.contains("-TAIL"));
        assert!(!bounded.contains("short presentation"));
        assert_eq!(event.output.as_text(), "short presentation");
    }

    #[test]
    fn per_tool_limits_override_agent_limits() {
        let text = "x".repeat(100);
        let mut done = done(text.clone(), false);
        done.output_limits = Some(ToolOutputLimits {
            max_lines: 10,
            max_bytes: 200,
        });
        let ctx = context(1, 20);

        smol::block_on(limit(&mut done, &ctx));

        assert_eq!(done.output.as_text(), text);
        assert!(done.model_output.is_none());
        assert!(done.output_ref.is_none());
    }

    #[test]
    fn per_tool_limits_bound_preview_and_preserve_full_stored_output() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 1_000, LARGE_BYTE_LIMIT);
        let text = (0..30)
            .map(|line| format!("line-{line}-{}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n");
        let limits = ToolOutputLimits {
            max_lines: 8,
            max_bytes: 360,
        };
        let mut done = done(text.clone(), false);
        done.output_limits = Some(limits);

        smol::block_on(limit(&mut done, &ctx));

        let output_ref = done.output_ref.as_ref().unwrap();
        let model_output = done.model_output.as_ref().unwrap();
        assert!(line_count(model_output) <= limits.max_lines);
        assert!(model_output.len() <= limits.max_bytes);
        assert!(line_count(&done.output.as_text()) <= limits.max_lines);
        assert!(done.output.as_text().len() <= limits.max_bytes);
        assert_eq!(
            store
                .read(session.id(), output_ref.id.clone(), 1, 2_000)
                .unwrap()
                .text,
            text
        );
    }

    #[test]
    fn zero_limits_fall_back_and_zero_config_is_clamped() {
        let ctx = context(12, 345);
        assert_eq!(
            effective_limits(
                Some(ToolOutputLimits {
                    max_lines: 0,
                    max_bytes: 0,
                }),
                &ctx.config,
            ),
            ToolOutputLimits {
                max_lines: 12,
                max_bytes: 345,
            }
        );

        let ctx = context(0, 0);
        assert_eq!(
            effective_limits(None, &ctx.config),
            ToolOutputLimits {
                max_lines: 1,
                max_bytes: 1,
            }
        );
    }

    #[test]
    fn tiny_positive_per_tool_limits_are_clamped_to_marker_minimums() {
        let ctx = context(12, 345);

        assert_eq!(
            effective_limits(
                Some(ToolOutputLimits {
                    max_lines: 1,
                    max_bytes: 1,
                }),
                &ctx.config,
            ),
            ToolOutputLimits {
                max_lines: MIN_PER_TOOL_OUTPUT_LINES,
                max_bytes: MIN_PER_TOOL_OUTPUT_BYTES,
            }
        );
    }

    #[test]
    fn tiny_per_tool_limits_preserve_the_recovery_id() {
        let temp = TempDir::new().unwrap();
        let (ctx, _store, _session) = stored_context(&temp, 1_000, LARGE_BYTE_LIMIT);
        let mut done = done("x".repeat(2_000), false);
        done.output_limits = Some(ToolOutputLimits {
            max_lines: 1,
            max_bytes: 1,
        });

        smol::block_on(limit(&mut done, &ctx));

        let id = done.output_ref.as_ref().unwrap().id.to_string();
        let model_output = done.model_output.as_ref().unwrap();
        assert!(model_output.contains(&id));
        assert!(model_output.len() <= MIN_PER_TOOL_OUTPUT_BYTES);
        assert!(line_count(model_output) <= MIN_PER_TOOL_OUTPUT_LINES);
    }

    #[test]
    fn stored_marker_and_suffix_fit_inside_both_limits() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 9, 420);
        let full_text = format!("first\n{}\nlast", "middle\n".repeat(100));
        let mut done =
            done(full_text.clone(), false).with_model_suffix(Some("model-only guidance".into()));

        smol::block_on(limit(&mut done, &ctx));

        let output_ref = done.output_ref.as_ref().unwrap();
        assert!(
            !done
                .model_output
                .as_ref()
                .unwrap()
                .contains("model-only guidance")
        );
        let model_text = done.composed_model_output();
        assert!(model_text.len() <= ctx.config.max_output_bytes);
        assert!(line_count(&model_text) <= ctx.config.max_output_lines);
        assert!(model_text.starts_with("first"));
        assert!(model_text.contains("last"));
        assert!(model_text.contains(&format!("Full output ID: {}", output_ref.id)));
        assert!(model_text.contains(&format!(
            "{}(output_id=",
            crate::tools::TOOL_OUTPUT_TOOL_NAME
        )));
        assert!(model_text.contains("pattern=\"...\""));
        assert!(model_text.ends_with("model-only guidance"));
        assert_eq!(model_text.matches("model-only guidance").count(), 1);

        let stored = store
            .read(session.id(), output_ref.id.clone(), 1, 2_000)
            .unwrap();
        assert_eq!(stored.text, format!("{full_text}\n\nmodel-only guidance"));
    }

    #[test]
    fn suffix_longer_than_the_limit_is_bounded_with_the_output() {
        let temp = TempDir::new().unwrap();
        let (ctx, _store, _session) = stored_context(&temp, 9, 420);
        let mut done =
            done("line\n".repeat(200), false).with_model_suffix(Some("guidance ".repeat(100)));

        smol::block_on(limit(&mut done, &ctx));

        assert!(done.model_suffix.is_none());
        let id = done.output_ref.as_ref().unwrap().id.to_string();
        let model_text = done.composed_model_output();
        assert!(model_text.contains(&id));
        assert!(model_text.contains("guidance"));
        assert!(model_text.len() <= ctx.config.max_output_bytes);
        assert!(line_count(&model_text) <= ctx.config.max_output_lines);
    }

    #[test]
    fn successful_output_over_threshold_is_stored_without_text_change() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 100, 20_000);
        let text = (0..10)
            .map(|line| format!("line-{line}-{}", "x".repeat(900)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut done = done(text.clone(), false);

        smol::block_on(limit(&mut done, &ctx));

        let output_ref = done.output_ref.as_ref().unwrap();
        assert!(done.model_output.is_none());
        assert_eq!(done.output.as_text(), text);
        assert_eq!(
            store
                .read(session.id(), output_ref.id.clone(), 1, 2_000)
                .unwrap()
                .text,
            text
        );
    }

    #[test]
    fn oversized_error_is_stored_and_bounded_without_changing_status() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 10, 300);
        let text = (0..20)
            .map(|line| format!("error-{line}-{}", "!".repeat(100)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut done = done(text.clone(), true);

        smol::block_on(limit(&mut done, &ctx));

        let output_ref = done.output_ref.as_ref().unwrap();
        assert!(done.is_error);
        assert!(done.model_output.as_ref().unwrap().len() <= ctx.config.max_output_bytes);
        assert_eq!(
            store
                .read(session.id(), output_ref.id.clone(), 1, 2_000)
                .unwrap()
                .text,
            text
        );
    }

    #[test]
    fn unavailable_persistence_still_bounds_successful_output() {
        let mut done = done("x".repeat(2_000), false);
        let mut ctx = context(10, 220);
        ctx.session_id = Some(SessionRef::generate());

        smol::block_on(limit(&mut done, &ctx));

        assert!(!done.is_error);
        assert!(done.output_ref.is_none());
        let model_output = done.model_output.unwrap();
        assert!(model_output.len() <= ctx.config.max_output_bytes);
        assert!(model_output.contains("Full output was unavailable"));
        assert!(model_output.contains("rerun this tool with narrower output"));
    }

    #[test]
    fn text_presentations_are_bounded_without_losing_metadata() {
        let text = TextOutput {
            text: "x".repeat(2_000),
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "keep metadata".into(),
            }]),
            state: Some(serde_json::json!({"cursor": 7})),
            lua_provenance: None,
        };
        let ctx = context(10, 320);

        for output in [
            ToolOutput::Plain(text.clone()),
            ToolOutput::Markdown(text.clone()),
            ToolOutput::ReadDir(text.clone()),
        ] {
            let mut done = done(String::new(), false);
            done.output = output;
            smol::block_on(limit(&mut done, &ctx));

            assert!(done.output.as_text().len() <= ctx.config.max_output_bytes);
            let text = match done.output {
                ToolOutput::Plain(text)
                | ToolOutput::Markdown(text)
                | ToolOutput::ReadDir(text) => text,
                _ => unreachable!(),
            };
            assert_eq!(text.instructions.unwrap()[0].content, "keep metadata");
            assert_eq!(text.state, Some(serde_json::json!({"cursor": 7})));
        }
    }

    #[test]
    fn index_presentation_is_bounded_without_losing_structured_state() {
        let state = serde_json::json!({"symbols": ["alpha", "omega"]});
        let mut done = done(String::new(), false);
        done.output = ToolOutput::Index(crate::IndexOutput::File {
            path: "/workspace/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            language: "rust".into(),
            skeleton: (0..100)
                .map(|line| format!("fn symbol_{line}() {{}}"))
                .collect::<Vec<_>>()
                .join("\n"),
            lines: vec![crate::IndexLine {
                output_line: 1,
                text: "fn symbol_0() {}".into(),
                semantic: crate::IndexLineSemantic::Item,
                body: Some("fn symbol_0() {}".into()),
                source_range: None,
            }],
            source_line_count: 100,
            parse_error: false,
            truncated: false,
            instructions: None,
            state: Some(state.clone()),
        });
        let ctx = context(8, 360);

        smol::block_on(limit(&mut done, &ctx));

        assert!(fits(
            &done.output.as_text(),
            ctx.config.max_output_lines,
            ctx.config.max_output_bytes
        ));
        let ToolOutput::Index(crate::IndexOutput::File {
            skeleton,
            lines,
            state: retained_state,
            ..
        }) = done.output
        else {
            unreachable!();
        };
        assert!(skeleton.contains("Tool output truncated"));
        assert_eq!(lines.len(), 1);
        assert_eq!(retained_state, Some(state));
    }

    /// The reported bug: a batch big enough to be bounded came back as `Plain`,
    /// so the card lost `entries` and drew the flattened `## tool` dump the
    /// model was handed, abridged to the three lines `other` allows.
    #[test]
    fn batch_presentation_is_bounded_without_losing_its_roster() {
        let mut done = done(String::new(), false);
        done.output = ToolOutput::Batch {
            entries: vec![BatchToolEntry {
                model_suffix: None,
                tool: "file_read".into(),
                effect: ToolEffect::ReadOnly,
                summary: "README.md".into(),
                status: BatchToolStatus::Success,
                input: None,
                raw_input: None,
                output: Some(ToolOutput::Plain("child body".into())),
                annotation: None,
            }],
            text: (0..400)
                .map(|line| format!("{line}: <p align=\"center\">"))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let ctx = context(8, 360);

        smol::block_on(limit(&mut done, &ctx));

        let ToolOutput::Batch { entries, text } = done.output else {
            panic!("{ROSTER_LOST}");
        };
        assert_eq!(entries.len(), 1, "{ROSTER_LOST}");
        assert_eq!(entries[0].tool, "file_read", "{ROSTER_LOST}");
        assert!(fits(
            &text,
            ctx.config.max_output_lines,
            ctx.config.max_output_bytes
        ));
        assert!(text.contains("Tool output truncated"));
    }

    fn big_read_code() -> ToolOutput {
        ToolOutput::ReadCode {
            path: "README.md".into(),
            start_line: 1,
            lines: (0..400).map(|line| format!("line-{line}")).collect(),
            total_lines: 400,
            instructions: None,
        }
    }

    fn big_grep_result() -> ToolOutput {
        ToolOutput::GrepResult {
            entries: (0..40)
                .map(|file| GrepFileEntry {
                    path: format!("src/file-{file}.rs"),
                    groups: vec![GrepMatchGroup {
                        lines: vec![GrepLine {
                            line_nr: 1,
                            text: "fn main() {}".into(),
                            is_match: true,
                        }],
                    }],
                })
                .collect(),
            capped: None,
        }
    }

    fn big_patch() -> ToolOutput {
        ToolOutput::Patch {
            files: (0..40)
                .map(|file| PatchedFile {
                    path: format!("src/file-{file}.rs"),
                    patch: "@@ -1 +1 @@\n-before\n+after".into(),
                    additions: 1,
                    deletions: 1,
                    truncated: false,
                })
                .collect(),
        }
    }

    /// The same defect one variant over: a result drawn from structured fields
    /// has no text to cut, and replacing it cost the code view, the match list
    /// and the diff the model never read anyway.
    #[test_case(big_read_code() ; "read_code")]
    #[test_case(big_grep_result() ; "grep_result")]
    #[test_case(big_patch() ; "patch")]
    fn structured_presentations_keep_their_shape_when_bounded(output: ToolOutput) {
        let shape = discriminant(&output);
        let mut done = done(String::new(), false);
        done.output = output;
        let ctx = context(8, 360);

        smol::block_on(limit(&mut done, &ctx));

        assert_eq!(discriminant(&done.output), shape, "{SHAPE_LOST}");
        let model_output = done.model_output.expect("bounded model output");
        assert!(fits(
            &model_output,
            ctx.config.max_output_lines,
            ctx.config.max_output_bytes
        ));
        assert!(model_output.contains("Tool output truncated"));
    }

    #[test]
    fn huge_instruction_blocks_are_bounded_everywhere_and_persisted_in_full() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 8, 360);
        let output = ToolOutput::Plain(TextOutput {
            text: "visible body".into(),
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "instruction".repeat(1_000),
            }]),
            state: Some(serde_json::json!({"cursor": 7})),
            lua_provenance: None,
        });
        let full_output = output.as_text();
        let mut done = done(String::new(), false);
        done.output = output;

        smol::block_on(limit(&mut done, &ctx));

        let presentation = done.output.as_text();
        assert!(fits(
            &presentation,
            ctx.config.max_output_lines,
            ctx.config.max_output_bytes
        ));
        assert!(presentation.contains("Full output ID:"));
        assert!(fits(
            done.model_output.as_deref().unwrap(),
            ctx.config.max_output_lines,
            ctx.config.max_output_bytes
        ));
        assert_eq!(
            crate::tools::interpreter_bridge::flatten(&done).unwrap(),
            presentation
        );
        let message = crate::types::tool_results(vec![done.clone()]);
        let model_content = match &message.content[0] {
            caudra_providers::ContentBlock::ToolResult { content, .. } => content,
            _ => unreachable!(),
        };
        assert!(fits(
            model_content,
            ctx.config.max_output_lines,
            ctx.config.max_output_bytes
        ));
        let output_ref = done.output_ref.unwrap();
        assert_eq!(
            store.load_text(session.id(), output_ref.id).unwrap(),
            full_output
        );
    }

    #[test]
    fn store_failure_does_not_turn_success_into_error() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(ToolOutputStore::with_max_bytes(
            StateDir::from_path(temp.path().to_path_buf()),
            10,
        ));
        let mut ctx = context(10, 220);
        ctx.session_id = Some(SessionRef::generate());
        ctx.tool_output_store = Some(store);
        let mut done = done("x".repeat(2_000), false);

        smol::block_on(limit(&mut done, &ctx));

        assert!(!done.is_error);
        assert!(done.output_ref.is_none());
        assert!(
            done.model_output
                .unwrap()
                .contains("Full output was unavailable")
        );
    }

    #[test]
    fn stored_output_belongs_only_to_current_session() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 10, 220);
        let mut done = done("owned".repeat(500), false);

        smol::block_on(limit(&mut done, &ctx));

        let output_ref = done.output_ref.unwrap();
        assert!(
            store
                .read(session.id(), output_ref.id.clone(), 1, 2_000)
                .is_ok()
        );
        assert!(
            store
                .read(SessionRef::generate().id(), output_ref.id, 1, 2_000)
                .is_err()
        );
    }

    #[test]
    fn pre_persisted_output_is_loaded_limited_and_not_written_twice() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 8, 360);
        let full_text = (0..30)
            .map(|line| format!("streamed-{line}-{}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut sink = store.begin(session.id()).unwrap();
        sink.append(&full_text).unwrap();
        let output_ref = sink.finish().unwrap();
        let mut done = done("placeholder".into(), false)
            .with_model_suffix(Some("stream stopped at the output limit".into()));
        done.output_ref = Some(output_ref.clone());
        done.model_output_from_ref = true;

        smol::block_on(limit(&mut done, &ctx));

        assert_eq!(done.output_ref.as_ref(), Some(&output_ref));
        assert!(!done.model_output_from_ref);
        assert!(done.output.as_text().len() <= ctx.config.max_output_bytes);
        assert!(done.output.as_text().contains("streamed-0-"));
        assert!(done.output.as_text().contains("streamed-29-"));
        assert!(done.output.as_text().contains("Full output ID:"));
        let model_text = done.composed_model_output();
        assert!(model_text.len() <= ctx.config.max_output_bytes);
        assert_eq!(
            model_text
                .matches("stream stopped at the output limit")
                .count(),
            1
        );
        assert_eq!(
            store.load_text(session.id(), output_ref.id).unwrap(),
            full_text
        );
        let session_dir = temp
            .path()
            .join("tool-output")
            .join(session.id().to_string());
        assert_eq!(fs::read_dir(session_dir).unwrap().count(), 1);
    }

    #[test]
    fn pre_persisted_output_with_huge_instructions_stores_complete_rendering() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 8, 360);
        let full_text = "streamed line\n".repeat(100);
        let instructions = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: "instruction".repeat(1_000),
        }];
        let expected = ToolOutput::Plain(TextOutput {
            text: full_text.clone(),
            instructions: Some(instructions.clone()),
            state: None,
            lua_provenance: None,
        })
        .as_text();
        let original_ref = store.put(session.id(), &full_text).unwrap();
        let mut done = done("placeholder".into(), false);
        done.output = ToolOutput::Plain(TextOutput {
            text: "placeholder".into(),
            instructions: Some(instructions),
            state: None,
            lua_provenance: None,
        });
        done.output_ref = Some(original_ref.clone());
        done.model_output_from_ref = true;

        smol::block_on(limit(&mut done, &ctx));

        let complete_ref = done.output_ref.as_ref().unwrap();
        assert_ne!(complete_ref.id, original_ref.id);
        assert_eq!(
            store
                .load_text(session.id(), complete_ref.id.clone())
                .unwrap(),
            expected
        );
        assert!(fits(
            &done.output.as_text(),
            ctx.config.max_output_lines,
            ctx.config.max_output_bytes
        ));
    }

    #[test]
    fn pre_persisted_load_failure_returns_explicit_fallback() {
        let temp = TempDir::new().unwrap();
        let (ctx, store, session) = stored_context(&temp, 20, 1_000);
        let output_ref = store.put(session.id(), "streamed text").unwrap();
        store.delete_session(session.id()).unwrap();
        let mut done = done("placeholder".into(), false);
        done.output_ref = Some(output_ref.clone());
        done.model_output_from_ref = true;

        smol::block_on(limit(&mut done, &ctx));

        let output = done.output.as_text();
        assert!(output.contains("Failed to load streamed tool output"));
        assert!(output.contains(&output_ref.id.to_string()));
        assert!(output.contains("does not exist for session"));
        assert!(!output.is_empty());
        assert!(done.is_error);
        assert!(done.output_ref.is_none());
        assert!(done.model_output.is_none());
    }
}
