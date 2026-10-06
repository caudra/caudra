//! The Projection view: the request section by section, each block under a
//! label naming what the provider receives, and the text the rows were painted
//! from kept whole for copying.

use std::borrow::Cow;
use std::fmt::Display;
use std::mem;

use caudra_providers::{
    ContentBlock, DocumentSource, Message, MessageKind, PDF_MEDIA_TYPE, Role, SteeringKind,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::{Projection, counted};
use crate::components::code_view::WrappedRows;
use crate::components::document_view::{Painted, UNBOUND_HINT};
use crate::components::modal::SEPARATOR;
use crate::components::{escape_terminal_controls, format_bytes, json_text};
use crate::highlight::TAB_SPACES;
use crate::markdown::source_to_lines;
use crate::theme::Theme;

/// Drawn beside every row of a section, in the section's colour.
pub(super) const BAR: &str = "▎";
/// The bar and the column that keeps it off the text.
pub(super) const GUTTER: u16 = 2;
pub(super) const SYSTEM_HEADER: &str = "SYSTEM";
pub(super) const TOOLS_HEADER: &str = "TOOLS";
pub(super) const USER_HEADER: &str = "USER";
const ASSISTANT_HEADER: &str = "ASSISTANT";
pub(super) const TOOL_NAME_KEY: &str = "name";
pub(super) const TOOL_DESCRIPTION_KEY: &str = "description";
const OBSERVATION_TAG: &str = "observation";
const MENTION_TAG: &str = "mention";
const SYNTHETIC_TAG: &str = "synthetic";
const RECOVERY_TAG: &str = "steering recovery";
const ADVISORY_TAG: &str = "steering advisory";
const COMPACTION_TAG: &str = "compaction summary";
const FILLER_TAG: &str = "filler";
const THINKING_LABEL: &str = "thinking";
const SIGNED_FLAG: &str = "signed";
const ENCRYPTED_FLAG: &str = "encrypted";
const INTERRUPTED_FLAG: &str = "interrupted";
const REDACTED_LABEL: &str = "redacted thinking";
const TOOL_USE_LABEL: &str = "tool_use";
const TOOL_RESULT_LABEL: &str = "tool_result";
const IMAGE_LABEL: &str = "image";
const DOCUMENT_LABEL: &str = "document";
const PAGE_NOUN: &str = "page";
pub(super) const BASE64_PAD: char = '=';
const BASE64_DIGIT_BITS: u64 = 6;
const BYTE_BITS: u64 = 8;
const RUNNING_PREFIX: &str = "The results of ";
const RUNNING_NOUN: &str = "running call";
const RUNNING_SUFFIX: &str = " join the next request.";
pub(crate) const UNPREPARED: &str = "No request has been prepared yet.";

/// The view painted to `width`, and the text its rows were painted from
/// before they were wrapped or escaped.
pub(super) fn paint(
    projection: Option<&Projection>,
    width: u16,
    theme: &Theme,
) -> (Painted, String) {
    let mut document = document(projection, theme);
    let source = document.take_source();
    (document.finish(width), source)
}

/// The text [`paint`] hands back beside the rows, which no wrap width
/// changes: what a copy taken before any frame has painted the view needs.
pub(super) fn source(projection: Option<&Projection>, theme: &Theme) -> String {
    document(projection, theme).take_source()
}

fn document<'a>(projection: Option<&Projection>, theme: &'a Theme) -> Document<'a> {
    let mut document = Document::new(theme);
    match projection {
        Some(projection) => document.request(projection),
        None => document.unprepared(),
    }
    document
}

/// The view before it is wrapped: its lines, the colour of the bar beside
/// each, the lines sections open on, and the text a copy hands over.
struct Document<'a> {
    theme: &'a Theme,
    lines: Vec<Line<'static>>,
    bars: Vec<Option<Style>>,
    anchors: Vec<usize>,
    source: String,
    /// The bar beside the rows being added: the open section's, or none on
    /// the rows between sections.
    bar: Option<Style>,
}

impl<'a> Document<'a> {
    fn new(theme: &'a Theme) -> Self {
        Self {
            theme,
            lines: Vec::new(),
            bars: Vec::new(),
            anchors: Vec::new(),
            source: String::new(),
            bar: None,
        }
    }

    fn request(&mut self, projection: &Projection) {
        let theme = self.theme;
        let prompt = &projection.prompt;
        self.section(
            Line::from(Span::styled(SYSTEM_HEADER, heading(theme.accent))),
            theme.accent,
        );
        self.markdown(&prompt.system, theme.assistant);

        let tools = prompt.tools.as_array().map_or(&[][..], Vec::as_slice);
        let header = Span::styled(TOOLS_HEADER, heading(theme.tool));
        self.section(tagged(header, [tools.len()], theme.tool_dim), theme.tool);
        for (index, tool) in tools.iter().enumerate() {
            if index > 0 {
                self.push(Line::default());
            }
            self.tool(tool);
        }

        for (index, message) in projection.messages.iter().enumerate() {
            self.message(index + 1, message);
        }
        if projection.running_calls > 0 {
            self.notice(running_notice(projection.running_calls));
        }
    }

    /// A tool as the request declares it: its name, its description as the
    /// markdown it is written in, and every other field, such as its input
    /// schema, as JSON under the field's name.
    fn tool(&mut self, tool: &Value) {
        let theme = self.theme;
        let text = |key: &str| tool.get(key).and_then(Value::as_str);
        if let Some(name) = text(TOOL_NAME_KEY) {
            self.push(Line::from(Span::styled(name.to_owned(), theme.tool)));
        }
        if let Some(description) = text(TOOL_DESCRIPTION_KEY).filter(|text| !text.is_empty()) {
            self.markdown(description, theme.assistant);
        }
        let fields = tool
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| !matches!(key.as_str(), TOOL_NAME_KEY | TOOL_DESCRIPTION_KEY));
        for (key, value) in fields {
            self.push(Line::from(Span::styled(key.clone(), theme.tool_dim)));
            self.json(value);
        }
    }

    fn message(&mut self, number: usize, message: &Message) {
        let theme = self.theme;
        let (role, style) = match message.role {
            Role::User => (USER_HEADER, theme.user),
            Role::Assistant => (ASSISTANT_HEADER, theme.assistant),
        };
        let header = Span::styled(format!("#{number} {role}"), heading(style));
        self.section(tagged(header, host_tags(message), theme.tool_dim), style);
        for block in &message.content {
            self.block(block);
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                for document in message
                    .tool_result_documents
                    .get(tool_use_id)
                    .into_iter()
                    .flatten()
                {
                    self.document(document);
                }
            }
        }
    }

    /// A PDF the request carries inside the tool result above it.
    fn document(&mut self, document: &DocumentSource) {
        let theme = self.theme;
        let details = [
            Some(PDF_MEDIA_TYPE.to_owned()),
            document.filename.clone(),
            Some(counted(document.page_count, PAGE_NOUN)),
            document
                .data
                .as_deref()
                .map(|data| format_bytes(decoded_len(data))),
        ];
        let label = Span::styled(DOCUMENT_LABEL, theme.accent);
        self.push(tagged(label, details.into_iter().flatten(), theme.tool_dim));
    }

    fn block(&mut self, block: &ContentBlock) {
        let theme = self.theme;
        let dim = theme.tool_dim;
        match block {
            ContentBlock::Text { text } => self.markdown(text, theme.assistant),
            ContentBlock::Thinking {
                thinking,
                signature,
                interrupted,
                responses,
                ..
            } => {
                let encrypted = responses
                    .as_ref()
                    .and_then(|reasoning| reasoning.encrypted_content.as_deref());
                let flags = [
                    is_present(signature.as_deref()).then_some(SIGNED_FLAG),
                    is_present(encrypted).then_some(ENCRYPTED_FLAG),
                    interrupted.then_some(INTERRUPTED_FLAG),
                ];
                let label = Span::styled(THINKING_LABEL, theme.thinking);
                self.push(tagged(label, flags.into_iter().flatten(), dim));
                self.markdown(thinking, theme.thinking);
            }
            ContentBlock::RedactedThinking { data } => {
                let label = Span::styled(REDACTED_LABEL, theme.thinking);
                self.push(tagged(label, [format_bytes(data.len() as u64)], dim));
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                let label = Span::styled(format!("{TOOL_USE_LABEL} {name}"), theme.tool);
                self.push(tagged(label, [id], dim));
                self.json(input);
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                let status = if *is_error {
                    theme.tool_error
                } else {
                    theme.tool_success
                };
                let label = Span::styled(TOOL_RESULT_LABEL, status);
                self.push(tagged(label, [tool_use_id], dim));
                self.plain(content, theme.assistant);
            }
            ContentBlock::Image { source } => {
                let details = [
                    source.media_type.mime().to_owned(),
                    format_bytes(decoded_len(&source.data)),
                ];
                let label = Span::styled(IMAGE_LABEL, theme.accent);
                self.push(tagged(label, details, dim));
            }
        }
    }

    fn markdown(&mut self, text: &str, style: Style) {
        for line in source_to_lines(text, style, self.theme) {
            self.push(line);
        }
    }

    /// Tool output is data, so it is shown as written with no markdown read
    /// into it.
    fn plain(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            self.push(Line::from(Span::styled(line.to_owned(), style)));
        }
    }

    fn json(&mut self, value: &Value) {
        let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
        for line in pretty.split('\n') {
            self.push(json_text::line(line));
        }
    }

    /// Opens a section on `header`, a blank row below the one before it.
    fn section(&mut self, header: Line<'static>, bar: Style) {
        if !self.lines.is_empty() {
            self.bar = None;
            self.push(Line::default());
        }
        self.bar = Some(bar);
        self.anchors.push(self.lines.len());
        self.push(header);
    }

    /// A row of the request, whose text joins what a copy hands over exactly
    /// as it was written.
    fn push(&mut self, line: Line<'static>) {
        for span in &line.spans {
            self.source.push_str(&span.content);
        }
        self.source.push('\n');
        self.show(line);
    }

    /// A row drawn and never copied.
    fn show(&mut self, line: Line<'static>) {
        self.lines.push(displayable(line));
        self.bars.push(self.bar);
    }

    /// Says something about the request rather than being part of it, so it
    /// stays out of the copy and out of every section.
    fn notice(&mut self, text: String) {
        self.bar = None;
        self.show(Line::default());
        self.show(Line::from(Span::styled(text, self.theme.status_dim)));
    }

    fn unprepared(&mut self) {
        self.show(Line::from(Span::styled(UNPREPARED, self.theme.status_dim)));
        self.show(Line::from(Span::styled(UNBOUND_HINT, self.theme.tool_dim)));
    }

    /// What a copy hands over: every row pushed, each but the last ended by a
    /// break.
    fn take_source(&mut self) -> String {
        let mut source = mem::take(&mut self.source);
        source.pop();
        source
    }

    fn finish(self, width: u16) -> Painted {
        let wrapped = WrappedRows::new(self.lines, 0, width);
        let gutter = wrapped
            .expand(self.bars)
            .into_iter()
            .map(|bar| bar.map_or_else(Line::default, |style| Line::from(Span::styled(BAR, style))))
            .collect();
        let anchors = wrapped.rows_of(&self.anchors);
        Painted::new(wrapped.lines(), gutter, anchors)
    }
}

/// A header or label row: what it names, then the details a reader tells it
/// apart by, dimmed.
fn tagged(
    head: Span<'static>,
    details: impl IntoIterator<Item = impl Display>,
    dim: Style,
) -> Line<'static> {
    let mut spans = vec![head];
    spans.extend(
        details
            .into_iter()
            .map(|detail| Span::styled(format!("{SEPARATOR}{detail}"), dim)),
    );
    Line::from(spans)
}

fn heading(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD)
}

/// What the host knows of a message and never tells the provider.
fn host_tags(message: &Message) -> impl Iterator<Item = &'static str> {
    let origin = match message.kind {
        MessageKind::Turn if message.display_text.as_deref().is_some_and(str::is_empty) => {
            Some(SYNTHETIC_TAG)
        }
        MessageKind::Turn => None,
        MessageKind::Observation => Some(OBSERVATION_TAG),
        MessageKind::Mention => Some(MENTION_TAG),
    };
    let steering = message
        .steering
        .as_ref()
        .map(|steering| match steering.kind {
            SteeringKind::Recovery => RECOVERY_TAG,
            SteeringKind::Advisory => ADVISORY_TAG,
        });
    [
        origin,
        steering,
        message.is_compaction_summary.then_some(COMPACTION_TAG),
        message.is_empty_padding().then_some(FILLER_TAG),
    ]
    .into_iter()
    .flatten()
}

fn is_present(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// The bytes a base64 payload decodes to, counted from its digits alone.
fn decoded_len(base64: &str) -> u64 {
    let digits = base64.trim_end_matches(BASE64_PAD).len() as u64;
    digits * BASE64_DIGIT_BITS / BYTE_BITS
}

/// Said under a request whose trailing calls are still running, in either
/// view, and never copied.
pub(super) fn running_notice(calls: usize) -> String {
    format!(
        "{RUNNING_PREFIX}{}{RUNNING_SUFFIX}",
        counted(calls, RUNNING_NOUN)
    )
}

/// A row as it may reach the terminal: control characters escaped and tabs
/// widened, and its indentation split into a span of its own so the rows it
/// wraps onto hang beneath it. A span with nothing to escape is kept as is.
pub(super) fn displayable(mut line: Line<'static>) -> Line<'static> {
    for span in &mut line.spans {
        if span.content.contains(char::is_control) {
            let widened = span.content.replace('\t', TAB_SPACES);
            span.content = Cow::Owned(escape_terminal_controls(&widened));
        }
    }
    if let Some(first) = line.spans.first_mut() {
        let text = first.content.as_ref();
        let indent = text.len() - text.trim_start_matches(' ').len();
        if indent > 0 && indent < text.len() {
            let rest = Span::styled(text[indent..].to_owned(), first.style);
            first.content = Cow::Owned(text[..indent].to_owned());
            line.spans.insert(1, rest);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::projection_modal::tests::{
        PARAMETER, SCHEMA_KEY, SYSTEM, TOOL_DESCRIPTION, TOOLS, projection, tool_schema,
    };
    use crate::theme;
    use caudra_providers::{ImageMediaType, ImageSource, ResponsesReasoning};
    use serde_json::json;
    use test_case::test_case;

    /// Wide enough that no fixture row wraps.
    const WIDE: u16 = 400;
    const NARROW: u16 = 24;
    const TEXT: &str = "hello";
    const RULE: &str = "empty-turn";
    const SIGNATURE: &str = "sig";
    const ENCRYPTED: &str = "opaque";
    const REDACTED: &str = "redacted-bytes";
    const CALL_ID: &str = "toolu_01";
    const TOOL: &str = "bash";
    const COMMAND_KEY: &str = "command";
    const THINKING_TEXT: &str = "**Plan** call `ls`\n- then\tread";
    /// A run of [`THINKING_TEXT`] no markdown touches.
    const THINKING_PLAIN: &str = " call ";
    const DELIMITER: &str = "**";
    const RESULT_TEXT: &str = "**not bold** `raw`";
    const CONTROL_TEXT: &str = "red\u{1b}[31m\tdone";
    const ESCAPED_TEXT: &str = "red\\u{1b}[31m  done";
    const ESCAPE: char = '\u{1b}';
    /// Three bytes of base64.
    const IMAGE_DATA: &str = "QUJD";
    const IMAGE_BYTES: u64 = 3;
    const PDF_URL: &str = "https://example.com/paper.pdf";
    const PDF_NAME: &str = "paper.pdf";
    /// `%PDF-1.7`, eight bytes of base64.
    const PDF_DATA: &str = "JVBERi0xLjc=";
    const PDF_BYTES: u64 = 8;
    const PDF_PAGES: usize = 3;
    const INDENT: &str = "    ";
    const INDENTED_TEXT: &str = "    alpha bravo charlie delta echo foxtrot golf hotel";
    const RUNNING_CALLS: usize = 2;
    const TAG_MISSING: &str = "a message header must carry the tags the host knows it by";
    const LABEL_MISSING: &str = "every block must open on the label naming it";
    const PIXELS_SHOWN: &str = "an image is a placeholder, never its pixels";
    const PDF_BYTES_SHOWN: &str = "a PDF is named by its size, never shown as its bytes";
    const NOT_VERBATIM: &str = "thinking must be shown as written, coloured as markdown source";
    const NOT_PLAIN: &str = "a tool result is data and must not be read as markdown";
    const NOT_COLOURED: &str = "a tool input must be coloured as JSON";
    const TOOL_INCOMPLETE: &str =
        "a tool must be declared whole: its name, its description and each other field";
    const NOT_ESCAPED: &str = "a control character must never reach the terminal";
    const COPY_ALTERED: &str = "a copy must hand over the text as it was written";
    const NOTICE_WRONG: &str = "running calls are announced once, after the last message";
    const ANCHOR_WRONG: &str = "each section must open on an anchored header behind its bar";
    const HANG_WRONG: &str = "a wrapped row must hang under the indentation it opens with";
    const ROW_MISSING: &str = "the fixture must paint the row the test reads";
    const SPAN_MISSING: &str = "the row must hold the span the test reads";

    fn paint_request(
        messages: Vec<Message>,
        running_calls: usize,
        width: u16,
    ) -> (Painted, String) {
        paint(
            Some(&projection(messages, running_calls)),
            width,
            &theme::current(),
        )
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn rows(messages: Vec<Message>) -> Vec<String> {
        texts(paint_request(messages, 0, WIDE).0.lines())
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn summary() -> Message {
        Message {
            is_compaction_summary: true,
            ..assistant(vec![ContentBlock::Text { text: TEXT.into() }])
        }
    }

    fn tool_result(content: &str, is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: CALL_ID.into(),
                content: content.into(),
                is_error,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    /// The row whose text is `text`, as painted.
    fn row<'a>(painted: &'a Painted, text: &str) -> &'a Line<'static> {
        painted
            .lines()
            .iter()
            .find(|line| line.to_string() == text)
            .unwrap_or_else(|| panic!("{ROW_MISSING}: {text:?} in {:?}", texts(painted.lines())))
    }

    fn span<'a>(line: &'a Line<'static>, text: &str) -> &'a Span<'static> {
        line.spans
            .iter()
            .find(|span| span.content == text)
            .unwrap_or_else(|| panic!("{SPAN_MISSING}: {text:?} in {line:?}"))
    }

    #[test_case(Message::user(TEXT.into()), USER_HEADER, &[] ; "turn")]
    #[test_case(Message::observation(TEXT.into()), USER_HEADER, &[OBSERVATION_TAG] ; "observation")]
    #[test_case(Message::mention(TEXT.into()), USER_HEADER, &[MENTION_TAG] ; "mention")]
    #[test_case(Message::synthetic(TEXT.into()), USER_HEADER, &[SYNTHETIC_TAG] ; "synthetic")]
    #[test_case(
        Message::steering(TEXT.into(), RULE, SteeringKind::Recovery), USER_HEADER,
        &[OBSERVATION_TAG, RECOVERY_TAG] ; "recovery_steering"
    )]
    #[test_case(
        Message::steering(TEXT.into(), RULE, SteeringKind::Advisory), USER_HEADER,
        &[OBSERVATION_TAG, ADVISORY_TAG] ; "advisory_steering"
    )]
    #[test_case(summary(), ASSISTANT_HEADER, &[COMPACTION_TAG] ; "compaction_summary")]
    #[test_case(Message::empty_marker(), ASSISTANT_HEADER, &[FILLER_TAG] ; "filler")]
    fn a_message_header_carries_its_host_tags(message: Message, role: &str, tags: &[&str]) {
        let header: String = [format!("#1 {role}")]
            .into_iter()
            .chain(tags.iter().map(|tag| format!("{SEPARATOR}{tag}")))
            .collect();

        assert!(
            rows(vec![message]).contains(&header),
            "{TAG_MISSING}: {header}"
        );
    }

    #[test]
    fn every_block_opens_on_its_label_row() {
        let blocks = vec![
            ContentBlock::Thinking {
                thinking: TEXT.into(),
                signature: Some(SIGNATURE.into()),
                duration_ms: None,
                interrupted: true,
                responses: None,
            },
            ContentBlock::Thinking {
                thinking: TEXT.into(),
                signature: None,
                duration_ms: None,
                interrupted: false,
                responses: Some(ResponsesReasoning {
                    item_id: CALL_ID.into(),
                    encrypted_content: Some(ENCRYPTED.into()),
                }),
            },
            ContentBlock::RedactedThinking {
                data: REDACTED.into(),
            },
            ContentBlock::tool_use(CALL_ID, TOOL, json!({})),
            ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into()),
            },
        ];
        let rows = rows(vec![assistant(blocks), tool_result(TEXT, false)]);

        for label in [
            [THINKING_LABEL, SIGNED_FLAG, INTERRUPTED_FLAG].join(SEPARATOR),
            [THINKING_LABEL, ENCRYPTED_FLAG].join(SEPARATOR),
            [REDACTED_LABEL, &format_bytes(REDACTED.len() as u64)].join(SEPARATOR),
            [&format!("{TOOL_USE_LABEL} {TOOL}"), CALL_ID].join(SEPARATOR),
            [TOOL_RESULT_LABEL, CALL_ID].join(SEPARATOR),
            [
                IMAGE_LABEL,
                ImageMediaType::Png.mime(),
                &format_bytes(IMAGE_BYTES),
            ]
            .join(SEPARATOR),
        ] {
            assert!(rows.contains(&label), "{LABEL_MISSING}: {label}");
        }
        assert!(
            !rows.iter().any(|row| row.contains(IMAGE_DATA)),
            "{PIXELS_SHOWN}"
        );
    }

    #[test]
    fn an_attached_pdf_is_shown_under_its_result_by_its_size() {
        let mut message = tool_result(TEXT, false);
        message.tool_result_documents.insert(
            CALL_ID.into(),
            vec![DocumentSource {
                url: PDF_URL.into(),
                filename: Some(PDF_NAME.into()),
                page_count: PDF_PAGES,
                data: Some(PDF_DATA.into()),
            }],
        );
        let rows = rows(vec![message]);
        let result = [TOOL_RESULT_LABEL, CALL_ID].join(SEPARATOR);
        let result = rows.iter().position(|row| *row == result).unwrap();

        assert_eq!(
            rows[result + 2],
            [
                DOCUMENT_LABEL,
                PDF_MEDIA_TYPE,
                PDF_NAME,
                &counted(PDF_PAGES, PAGE_NOUN),
                &format_bytes(PDF_BYTES),
            ]
            .join(SEPARATOR)
        );
        assert!(
            !rows.iter().any(|row| row.contains(PDF_DATA)),
            "{PDF_BYTES_SHOWN}"
        );
    }

    #[test]
    fn thinking_is_shown_as_written_on_the_thinking_colour() {
        let theme = theme::current();
        let thinking = ContentBlock::thinking(THINKING_TEXT.into(), None);
        let (painted, _) = paint_request(vec![assistant(vec![thinking])], 0, WIDE);
        let rows = texts(painted.lines());
        let label = rows.iter().position(|row| row == THINKING_LABEL).unwrap();

        let shown = THINKING_TEXT.replace('\t', TAB_SPACES);
        assert_eq!(
            rows[label + 1..],
            shown.split('\n').collect::<Vec<_>>(),
            "{NOT_VERBATIM}"
        );
        let first = &painted.lines()[label + 1];
        assert_eq!(
            span(first, THINKING_PLAIN).style.fg,
            theme.thinking.fg,
            "{NOT_VERBATIM}"
        );
        assert_eq!(
            span(first, DELIMITER).style.fg,
            theme.tool_dim.fg,
            "{NOT_VERBATIM}"
        );
    }

    #[test_case(false ; "success")]
    #[test_case(true  ; "error")]
    fn a_tool_input_is_coloured_json_and_its_result_stays_plain(is_error: bool) {
        let theme = theme::current();
        let call = ContentBlock::tool_use(CALL_ID, TOOL, json!({ COMMAND_KEY: RESULT_TEXT }));
        let (painted, _) = paint_request(
            vec![assistant(vec![call]), tool_result(RESULT_TEXT, is_error)],
            0,
            WIDE,
        );

        let quoted_key = format!("\"{COMMAND_KEY}\"");
        let input = painted
            .lines()
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == quoted_key)
            .expect(NOT_COLOURED);
        assert_eq!(input.style, theme.accent, "{NOT_COLOURED}");

        let status = if is_error {
            theme.tool_error
        } else {
            theme.tool_success
        };
        let label = row(&painted, &[TOOL_RESULT_LABEL, CALL_ID].join(SEPARATOR));
        assert_eq!(label.spans[0].style, status);
        assert!(
            row(&painted, RESULT_TEXT)
                .spans
                .iter()
                .all(|span| span.style == theme.assistant),
            "{NOT_PLAIN}"
        );
    }

    /// Each tool opens on its name, then its description as the markdown
    /// source it was written in, then every other field as JSON under the
    /// field's name. A blank row keeps one tool off the next.
    #[test]
    fn every_tool_is_declared_whole_under_its_name() {
        let theme = theme::current();
        let (painted, source) = paint_request(Vec::new(), 0, WIDE);
        let rows = texts(painted.lines());
        let schema = serde_json::to_string_pretty(&tool_schema()).unwrap();
        let tools: Vec<Vec<&str>> = TOOLS
            .iter()
            .map(|&name| {
                [name]
                    .into_iter()
                    .chain(TOOL_DESCRIPTION.split('\n'))
                    .chain([SCHEMA_KEY])
                    .chain(schema.split('\n'))
                    .collect()
            })
            .collect();
        let declared = tools.join(&"");
        let header = format!("{TOOLS_HEADER}{SEPARATOR}{}", TOOLS.len());
        let first = rows
            .iter()
            .position(|row| *row == header)
            .expect(ROW_MISSING)
            + 1;

        assert_eq!(
            rows[first..first + declared.len()],
            declared,
            "{TOOL_INCOMPLETE}"
        );
        assert!(source.contains(&declared.join("\n")), "{COPY_ALTERED}");
        assert_eq!(
            row(&painted, TOOLS[0]).spans[0].style,
            theme.tool,
            "{TOOL_INCOMPLETE}"
        );
        assert_eq!(
            span(&painted.lines()[first + 1], DELIMITER).style.fg,
            theme.tool_dim.fg,
            "{NOT_VERBATIM}"
        );
        let quoted_key = format!("\"{PARAMETER}\"");
        let key = painted
            .lines()
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == quoted_key)
            .expect(NOT_COLOURED);
        assert_eq!(key.style, theme.accent, "{NOT_COLOURED}");
    }

    /// An MCP tool may declare no description, or an empty one. Neither draws
    /// a row, and the fields beside it are still shown.
    #[test_case(json!({ TOOL_NAME_KEY: TOOL, TOOL_DESCRIPTION_KEY: "" }), &[TOOL] ; "empty")]
    #[test_case(
        json!({ TOOL_NAME_KEY: TOOL, TOOL_DESCRIPTION_KEY: null, SCHEMA_KEY: {} }),
        &[TOOL, SCHEMA_KEY, "{}"] ; "null"
    )]
    fn a_tool_without_a_description_draws_no_row_for_it(tool: Value, expected: &[&str]) {
        let theme = theme::current();
        let mut document = Document::new(&theme);

        document.tool(&tool);

        assert_eq!(texts(&document.lines), expected, "{TOOL_INCOMPLETE}");
    }

    #[test]
    fn controls_are_escaped_on_screen_and_kept_in_the_copy() {
        let (painted, source) = paint_request(vec![tool_result(CONTROL_TEXT, false)], 0, WIDE);
        let rows = texts(painted.lines());

        assert!(rows.contains(&ESCAPED_TEXT.to_owned()), "{NOT_ESCAPED}");
        assert!(
            !rows.iter().any(|row| row.contains(ESCAPE)),
            "{NOT_ESCAPED}"
        );
        assert!(source.contains(CONTROL_TEXT), "{COPY_ALTERED}");
    }

    #[test_case(0             ; "settled")]
    #[test_case(RUNNING_CALLS ; "running")]
    fn running_calls_are_announced_after_the_last_message(running_calls: usize) {
        let (painted, source) =
            paint_request(vec![Message::user(TEXT.into())], running_calls, WIDE);
        let notice = running_notice(RUNNING_CALLS);

        let rows = texts(painted.lines());
        assert_eq!(
            rows.last() == Some(&notice),
            running_calls > 0,
            "{NOTICE_WRONG}"
        );
        assert!(!source.contains(&notice), "{NOTICE_WRONG}");
    }

    #[test]
    fn an_unprepared_request_is_a_notice_with_nothing_to_copy() {
        let (painted, source) = paint(None, WIDE, &theme::current());

        assert_eq!(texts(painted.lines()), [UNPREPARED, UNBOUND_HINT]);
        assert!(source.is_empty());
    }

    /// The copy reads exactly like the view does, sections, labels and all,
    /// once nothing needs escaping or wrapping.
    #[test]
    fn sections_open_on_anchored_headers_behind_their_bars() {
        let (painted, source) = paint_request(vec![Message::user(TEXT.into()), summary()], 0, WIDE);
        let rows = texts(painted.lines());
        let anchors = painted.anchors();
        let headers: Vec<&str> = anchors.iter().map(|&row| rows[row].as_str()).collect();

        assert_eq!(
            headers,
            [
                SYSTEM_HEADER.to_owned(),
                format!("{TOOLS_HEADER}{SEPARATOR}{}", TOOLS.len()),
                format!("#1 {USER_HEADER}"),
                format!("#2 {ASSISTANT_HEADER}{SEPARATOR}{COMPACTION_TAG}"),
            ],
            "{ANCHOR_WRONG}"
        );
        for (row, cell) in texts(painted.gutter()).iter().enumerate() {
            let between_sections = anchors.contains(&(row + 1));
            let expected = if between_sections { "" } else { BAR };
            assert_eq!(cell, expected, "{ANCHOR_WRONG}: row {row}");
        }
        assert!(source.starts_with(&format!("{SYSTEM_HEADER}\n{SYSTEM}")));
        assert_eq!(source, rows.join("\n"), "{COPY_ALTERED}");
    }

    #[test]
    fn a_wrapped_row_hangs_under_its_indentation() {
        let (painted, source) = paint_request(vec![tool_result(INDENTED_TEXT, false)], 0, NARROW);
        let rows = texts(painted.lines());
        let label = rows
            .iter()
            .position(|row| row.starts_with(TOOL_RESULT_LABEL))
            .unwrap();
        let block = &rows[label + 1..];

        assert!(block.len() > 1, "{HANG_WRONG}: {rows:?}");
        for row in block {
            assert!(row.starts_with(INDENT), "{HANG_WRONG}: {row:?}");
            assert!(
                !row[INDENT.len()..].starts_with(' '),
                "{HANG_WRONG}: {row:?}"
            );
        }
        assert!(source.contains(INDENTED_TEXT), "{COPY_ALTERED}");
    }

    #[test_case(""     => 0 ; "empty")]
    #[test_case("QQ==" => 1 ; "two_pad_digits")]
    #[test_case("QUI=" => 2 ; "one_pad_digit")]
    #[test_case("QUJD" => 3 ; "unpadded_group")]
    #[test_case("QUI"  => 2 ; "pad_left_off")]
    fn base64_is_measured_by_what_it_decodes_to(base64: &str) -> u64 {
        decoded_len(base64)
    }
}
