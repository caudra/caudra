//! Turns `@path` mentions into hidden context ahead of the user's turn.
//!
//! Each mention becomes one synthetic message carrying the file's contents, so
//! the visible transcript keeps the short `@src/main.rs:L12-L20` the user typed
//! while the model is handed what it names. Reading goes through Workcell's
//! `file_read` rather than a private reader, so truncation, binary sniffing and
//! line-number prefixes match what the model sees when it calls the tool
//! itself.
//!
//! Content is resolved once, at send time, and then lives in history like any
//! other message: a later turn sees what the file said when it was mentioned,
//! not what it says now.

use std::path::Path;
use std::sync::Arc;

use caudra_providers::{ImageMediaType, Message};
use caudra_workspace::{ReadTextRequest, ResourceKind, ResourceSelector, TextRange, WorkspacePath};
use serde_json::json;

use crate::mentions::{Mention, MentionTarget};
use crate::remote_project_context::RemoteProjectContext;
use crate::tools::image_bytes;
use crate::tools::{ToolContext, ToolRegistry};

const READ_TOOL: &str = "file_read";
/// Total inlined bytes one turn may spend across every kind of attachment. One
/// ceiling rather than one per kind, so a prompt that mixes files and commits
/// cannot quietly spend twice what either alone is allowed.
pub const MAX_TOTAL_BYTES: usize = 96 * 1024;
pub(crate) const BUDGET_ERROR: &str = "not inlined: this turn's mention budget was already spent";
const UNAVAILABLE_ERROR: &str = "not inlined: the file_read tool is not registered";
const NO_VISION_ERROR: &str = "not inlined: this model cannot read images";

/// What a mention needs to resolve. The two contexts differ only in their file
/// tracker, which is the whole point: Workcell records every successful
/// `file_read` against the tracker it is handed, without regard for `offset`,
/// so a slice must be given one whose records are thrown away.
pub struct Resolution<'a> {
    pub root: &'a Path,
    pub registry: &'a ToolRegistry,
    /// Records the read, letting a later `file_edit` proceed.
    pub whole_file: &'a ToolContext,
    /// Discards the read: seeing 20 lines is not grounds for editing the file.
    pub slice: &'a ToolContext,
    pub vision: bool,
    pub remote_context: Option<&'a Arc<RemoteProjectContext>>,
}

pub async fn build(
    mentions: &[Mention],
    resolution: Resolution<'_>,
    budget: &mut usize,
) -> Vec<Message> {
    let mut seen: Vec<&Mention> = Vec::with_capacity(mentions.len());
    let mut messages = Vec::new();
    for mention in mentions {
        if seen
            .iter()
            .any(|other| other.target == mention.target && other.lines == mention.lines)
        {
            continue;
        }
        seen.push(mention);
        messages.push(resolve(mention, &resolution, budget).await);
    }
    messages
}

async fn resolve(mention: &Mention, resolution: &Resolution<'_>, budget: &mut usize) -> Message {
    if matches!(mention.target, MentionTarget::Remote(_)) {
        return resolve_remote(mention, resolution, budget).await;
    }
    let Some(path) = mention.local_path() else {
        return note(mention, "not inlined: invalid client-local attachment");
    };
    if is_image(path) {
        return image(mention, resolution.root, resolution.vision);
    }
    if *budget == 0 {
        return note(mention, BUDGET_ERROR);
    }
    let Some(tool) = resolution.registry.get(READ_TOOL) else {
        return note(mention, UNAVAILABLE_ERROR);
    };
    let mut input = json!({ "filePath": path.to_string_lossy() });
    if let Some(lines) = &mention.lines {
        input["offset"] = json!(lines.start());
        input["limit"] = json!(lines.end() - lines.start() + 1);
    }
    let Some(invocation) = tool.try_parse(&input) else {
        return note(mention, "not inlined: the path could not be read");
    };
    let context = match mention.is_whole_file() {
        true => resolution.whole_file,
        false => resolution.slice,
    };
    let result = invocation.execute(context).await;
    if result.is_error {
        return note(mention, &result.output.err().unwrap_or_default());
    }
    // A read renders its own body now, so the model output is set only when a
    // remote hands one over. Either way the mention inlines what the model
    // would have read had it made the call itself.
    let Ok(output) = result.output else {
        return note(mention, "not inlined: the read returned nothing");
    };
    let body = result.model_output.unwrap_or_else(|| output.as_text());
    *budget = budget.saturating_sub(body.len());
    Message::mention(format!("{}\n{body}\n</file>", open_tag(mention)))
}

async fn resolve_remote(
    mention: &Mention,
    resolution: &Resolution<'_>,
    budget: &mut usize,
) -> Message {
    let Some(path) = mention.remote_path() else {
        return note(mention, "not inlined: invalid remote path");
    };
    if is_image_name(path.as_str()) {
        return remote_image(mention, resolution, path).await;
    }
    if *budget == 0 {
        return note(mention, BUDGET_ERROR);
    }
    let Some(session) = resolution.whole_file.workspace_session.as_ref() else {
        return note(mention, "not inlined: the remote workspace is unavailable");
    };
    let Some(service) = session.workspace().services().read.as_ref() else {
        return note(mention, "not inlined: remote reads are unavailable");
    };
    let resource = match service
        .resolve(session.binding(), session.cursor(), path)
        .await
    {
        Ok(resource) => resource,
        Err(_) => {
            return note(
                mention,
                "not inlined: the remote path could not be resolved",
            );
        }
    };
    if resource.kind != ResourceKind::File || resource.path.as_ref() != Some(path) {
        return note(mention, "not inlined: the remote path is not a file");
    }
    let resource_id = resource.scope.resource_id().clone();
    let stat = match service
        .stat(
            session.binding(),
            session.cursor(),
            &ResourceSelector::Id(resource_id.clone()),
        )
        .await
    {
        Ok(stat) => stat,
        Err(_) => {
            return note(
                mention,
                "not inlined: the remote file could not be inspected",
            );
        }
    };
    if stat.kind != ResourceKind::File
        || stat.path.as_ref() != Some(path)
        || stat.scope.resource_id() != &resource_id
        || stat.revision != resource.revision
    {
        return note(
            mention,
            "not inlined: the remote file changed before it was read",
        );
    }
    let range = mention.lines.as_ref().and_then(|lines| {
        Some(TextRange {
            start_line: u32::try_from(*lines.start()).ok()?,
            end_line: Some(u32::try_from(*lines.end()).ok()?),
        })
    });
    let max_bytes = u32::try_from((*budget).min(MAX_TOTAL_BYTES)).unwrap_or(MAX_TOTAL_BYTES as u32);
    let content = match service
        .read_text(
            session.binding(),
            session.cursor(),
            &ReadTextRequest {
                resource: ResourceSelector::Id(resource_id.clone()),
                range,
                byte_offset: 0,
                max_bytes,
            },
        )
        .await
    {
        Ok(content) => content,
        Err(_) => return note(mention, "not inlined: the remote file could not be read"),
    };
    if content.path != *path
        || content.resource_id != resource_id
        || stat.revision.as_ref() != Some(&content.revision)
        || content.text.len() > max_bytes as usize
    {
        return note(
            mention,
            "not inlined: the remote read did not match the requested file",
        );
    }
    *budget = budget.saturating_sub(content.text.len());
    let nested = resolution
        .remote_context
        .into_iter()
        .flat_map(|context| {
            crate::agent::find_remote_nested_instructions(
                context,
                path,
                &resolution.whole_file.loaded_instructions,
            )
        })
        .map(|(source, content)| {
            format!(
                "<instructions scope=\"project\" path=\"{source}\">\n{content}\n</instructions>\n"
            )
        })
        .collect::<String>();
    let truncation = if content.truncated {
        "\n[remote read truncated]"
    } else {
        ""
    };
    Message::mention(format!(
        "{nested}{}\n{}{truncation}\n</file>",
        open_tag(mention),
        content.text
    ))
}

async fn remote_image(
    mention: &Mention,
    resolution: &Resolution<'_>,
    path: &WorkspacePath,
) -> Message {
    if !resolution.vision {
        return note(mention, NO_VISION_ERROR);
    }
    let Some(session) = resolution.whole_file.workspace_session.as_ref() else {
        return note(mention, "not inlined: the remote workspace is unavailable");
    };
    let prepared = match image_bytes::resolve_remote(session, path).await {
        Ok(resource) => image_bytes::read_remote(session, &resource).await,
        Err(error) => Err(error),
    };
    match prepared {
        Ok(prepared) => Message::user_display_with_images(
            format!("{}\n</file>", open_tag(mention)),
            String::new(),
            vec![prepared.source],
        ),
        Err(error) => note(mention, &error.message),
    }
}

/// An image is handed over as a real image block when the model can see one.
/// The read budget does not apply: images are billed by the provider as tiles,
/// not by the byte, and `prepare` already shrinks them to fit.
fn image(mention: &Mention, root: &Path, vision: bool) -> Message {
    if !vision {
        return note(mention, NO_VISION_ERROR);
    }
    let Some(local_path) = mention.local_path() else {
        return note(mention, "not inlined: invalid client-local image path");
    };
    let path = root.join(local_path);
    match image_bytes::prepare(&path.to_string_lossy()) {
        Ok(prepared) => Message::user_display_with_images(
            format!("{}\n</file>", open_tag(mention)),
            String::new(),
            vec![prepared.source],
        ),
        Err(error) => note(mention, &error.message),
    }
}

fn open_tag(mention: &Mention) -> String {
    let path = mention.display_path();
    match &mention.lines {
        Some(lines) => format!(
            "<file path=\"{path}\" lines=\"{}-{}\">",
            lines.start(),
            lines.end()
        ),
        None => format!("<file path=\"{path}\">"),
    }
}

/// A mention that produced no content still reaches the model, so it can tell
/// "the file was empty" from "the file was never read".
fn note(mention: &Mention, error: &str) -> Message {
    let open = open_tag(mention);
    let open = open.trim_end_matches('>');
    Message::mention(format!("{open} error=\"{}\" />", error.replace('"', "'")))
}

fn is_image(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();
    ImageMediaType::ALL
        .into_iter()
        .any(|media| media.mime().trim_start_matches("image/") == extension)
        || extension == "jpg"
}

fn is_image_name(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            let extension = extension.to_ascii_lowercase();
            ImageMediaType::ALL
                .into_iter()
                .any(|media| media.mime().trim_start_matches("image/") == extension)
                || extension == "jpg"
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::ContentBlock;
    use std::path::PathBuf;
    use test_case::test_case;

    /// Whether a resolved message carries an image the provider will be sent.
    fn has_image(message: &Message) -> bool {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }

    const WHOLE: &str = "src/main.rs";

    fn text_of(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test_case(None, "<file path=\"src/main.rs\">" ; "whole_file")]
    #[test_case(Some(12..=20), "<file path=\"src/main.rs\" lines=\"12-20\">" ; "line_range")]
    fn open_tag_names_the_slice(lines: Option<std::ops::RangeInclusive<usize>>, expected: &str) {
        assert_eq!(open_tag(&Mention::new(WHOLE, lines)), expected);
    }

    #[test]
    fn a_note_is_self_closing_and_quotes_safely() {
        let message = note(&Mention::new(WHOLE, None), "he said \"no\"");
        assert_eq!(
            text_of(&message),
            "<file path=\"src/main.rs\" error=\"he said 'no'\" />"
        );
        assert_eq!(message.display_text.as_deref(), Some(""));
    }

    #[test_case("a.png", true ; "png")]
    #[test_case("a.JPG", true ; "uppercase_jpg")]
    #[test_case("a.jpeg", true ; "jpeg")]
    #[test_case("a.webp", true ; "webp")]
    #[test_case("a.rs", false ; "source_file")]
    #[test_case("a", false ; "no_extension")]
    fn image_detection_follows_the_extension(path: &str, expected: bool) {
        assert_eq!(is_image(&PathBuf::from(path)), expected);
    }

    #[test]
    fn a_model_without_vision_is_told_why_an_image_is_missing() {
        let message = image(&Mention::new("shot.png", None), Path::new("."), false);
        assert!(text_of(&message).contains(NO_VISION_ERROR));
        assert!(!has_image(&message));
    }

    #[test]
    fn a_missing_image_degrades_to_a_note_rather_than_an_empty_block() {
        let message = image(&Mention::new("absent.png", None), Path::new("."), true);
        assert!(!has_image(&message));
        assert!(text_of(&message).starts_with("<file path=\"absent.png\" error="));
    }
}
