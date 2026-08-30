use std::sync::{Arc, LazyLock};

use maki_storage::StateDir;
use maki_storage::sessions::persisted_session_ids;
use maki_storage::tool_outputs::{ToolOutputRef, ToolOutputStore, line_count};
use tracing::warn;

use maki_config::{MIN_PER_TOOL_OUTPUT_BYTES, MIN_PER_TOOL_OUTPUT_LINES};

use crate::tools::ToolContext;
use crate::{TextOutput, ToolDoneEvent, ToolOutput, ToolOutputLimits};

const PERSIST_THRESHOLD_BYTES: usize = 8 * 1024;
const READ_LIMIT: usize = 200;

static DEFAULT_STORE: LazyLock<Option<Arc<ToolOutputStore>>> = LazyLock::new(|| {
    StateDir::resolve()
        .map(|state_dir| {
            let store = ToolOutputStore::new(state_dir.clone());
            match persisted_session_ids(&state_dir) {
                Ok(session_ids) => {
                    if let Err(error) = store.cleanup_orphans(&session_ids) {
                        warn!(%error, "failed to clean up orphaned tool outputs");
                    }
                }
                Err(error) => warn!(%error, "failed to enumerate sessions for tool output cleanup"),
            }
            Arc::new(store)
        })
        .map_err(|error| warn!(%error, "tool output store unavailable"))
        .ok()
});

pub(crate) fn default_store() -> Option<Arc<ToolOutputStore>> {
    DEFAULT_STORE.clone()
}

pub(crate) async fn limit(done: &mut ToolDoneEvent, ctx: &ToolContext) {
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

    let output_ref = if pre_persisted && done.output.instructions().is_some() {
        let output_ref = persist(ctx, full_model_output).await;
        if output_ref.is_some() {
            done.output_ref = output_ref.clone();
        }
        output_ref
    } else if pre_persisted {
        done.output_ref.clone()
    } else {
        let output_ref = persist(ctx, full_model_output).await;
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
    let preview_body = preview_body(
        &source,
        &marker,
        &trailer,
        limits.max_lines,
        limits.max_bytes,
    );
    let model_output = preview_body + &trailer;
    done.model_output = Some(hard_bound(
        &model_output,
        limits.max_lines,
        limits.max_bytes,
    ));
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
            let id = output_ref.id;
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
    config: &maki_config::AgentConfig,
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

async fn persist(ctx: &ToolContext, text: String) -> Option<ToolOutputRef> {
    let (Some(session_id), Some(store)) = (&ctx.session_id, &ctx.tool_output_store) else {
        return None;
    };
    let session_id = session_id.clone();
    let storage_session_id = session_id.id();
    let store = Arc::clone(store);
    match smol::unblock(move || store.put(storage_session_id, &text)).await {
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
            "[Tool output truncated. Full output ID: {id}. Use tool_output_read(output_id=\"{id}\", offset=1, limit={READ_LIMIT}) or tool_output_grep(output_id=\"{id}\", pattern=\"...\").]",
            id = output_ref.id,
        ),
        None => "[Tool output truncated. Full output was unavailable; rerun this tool with narrower output.]".into(),
    }
}

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

    if let Some(text) = text_output_mut(output) {
        let source = text.text.clone();
        let trailer = rendered.strip_prefix(&source).unwrap_or_default();
        let ending = format!("{marker}{trailer}");
        if fits(&ending, max_lines, max_bytes) {
            text.text = preview_body(&source, marker, trailer, max_lines, max_bytes);
            return;
        }

        text.text = preview_body(&rendered, marker, "", max_lines, max_bytes);
        text.instructions = None;
        return;
    }

    *output = ToolOutput::Plain(preview_body(&rendered, marker, "", max_lines, max_bytes).into());
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

    use maki_storage::StateDir;
    use maki_storage::id::SessionRef;
    use maki_storage::tool_outputs::ToolOutputStore;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::InstructionBlock;

    const LARGE_BYTE_LIMIT: usize = 100_000;

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
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
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
                .read(session.id(), output_ref.id, 1, 2_000)
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
        let model_output = done.model_output.as_ref().unwrap();
        assert!(model_output.len() <= ctx.config.max_output_bytes);
        assert!(line_count(model_output) <= ctx.config.max_output_lines);
        assert!(model_output.starts_with("first"));
        assert!(model_output.contains("last"));
        assert!(model_output.contains(&format!("Full output ID: {}", output_ref.id)));
        assert!(model_output.contains("tool_output_read(output_id="));
        assert!(model_output.contains("tool_output_grep(output_id="));
        assert!(model_output.ends_with("model-only guidance"));
        assert_eq!(model_output.matches("model-only guidance").count(), 1);

        let stored = store.read(session.id(), output_ref.id, 1, 2_000).unwrap();
        assert_eq!(stored.text, format!("{full_text}\n\nmodel-only guidance"));
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
                .read(session.id(), output_ref.id, 1, 2_000)
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
                .read(session.id(), output_ref.id, 1, 2_000)
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
            maki_providers::ContentBlock::ToolResult { content, .. } => content,
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
        assert!(store.read(session.id(), output_ref.id, 1, 2_000).is_ok());
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
        let model_output = done.model_output.as_ref().unwrap();
        assert!(model_output.len() <= ctx.config.max_output_bytes);
        assert_eq!(
            model_output
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
            store.load_text(session.id(), complete_ref.id).unwrap(),
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
