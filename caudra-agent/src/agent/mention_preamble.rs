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

use caudra_providers::{ImageMediaType, Message};
use serde_json::json;

use crate::mentions::Mention;
use crate::tools::image_bytes;
use crate::tools::{ToolContext, ToolRegistry};

const READ_TOOL: &str = "file_read";
/// Total inlined bytes one turn may spend. A mention past this is announced
/// rather than dropped, so the model never silently reasons from a gap.
const MAX_TOTAL_BYTES: usize = 96 * 1024;
const BUDGET_ERROR: &str = "not inlined: this turn's mention budget was already spent";
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
}

pub async fn build(mentions: &[Mention], resolution: Resolution<'_>) -> Vec<Message> {
    let mut seen: Vec<&Mention> = Vec::with_capacity(mentions.len());
    let mut budget = MAX_TOTAL_BYTES;
    let mut messages = Vec::new();
    for mention in mentions {
        if seen
            .iter()
            .any(|other| other.path == mention.path && other.lines == mention.lines)
        {
            continue;
        }
        seen.push(mention);
        messages.push(resolve(mention, &resolution, &mut budget).await);
    }
    messages
}

async fn resolve(mention: &Mention, resolution: &Resolution<'_>, budget: &mut usize) -> Message {
    if is_image(&mention.path) {
        return image(mention, resolution.root, resolution.vision);
    }
    if *budget == 0 {
        return note(mention, BUDGET_ERROR);
    }
    let Some(tool) = resolution.registry.get(READ_TOOL) else {
        return note(mention, UNAVAILABLE_ERROR);
    };
    let mut input = json!({ "filePath": mention.path.to_string_lossy() });
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
    let Some(body) = result.model_output else {
        return note(mention, "not inlined: the read returned nothing");
    };
    *budget = budget.saturating_sub(body.len());
    Message::mention(format!("{}\n{body}\n</file>", open_tag(mention)))
}

/// An image is handed over as a real image block when the model can see one.
/// The read budget does not apply: images are billed by the provider as tiles,
/// not by the byte, and `prepare` already shrinks them to fit.
fn image(mention: &Mention, root: &Path, vision: bool) -> Message {
    if !vision {
        return note(mention, NO_VISION_ERROR);
    }
    let path = root.join(&mention.path);
    match image_bytes::prepare(&path.to_string_lossy()) {
        Ok(prepared) => Message::user_display_with_images(
            format!("{}\n</file>", open_tag(mention)),
            String::new(),
            vec![prepared.source],
        ),
        Err(error) => note(mention, &error),
    }
}

fn open_tag(mention: &Mention) -> String {
    let path = mention.path.display();
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
