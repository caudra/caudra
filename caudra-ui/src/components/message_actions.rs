use crate::components::docs_link::DocsTopic;
use crate::components::list_picker::{ListPicker, NO_MATCHES, PickerAction, PickerItem};
use crate::components::messages::MessageDetails;
use crate::components::tooltip::Tip;
use crate::components::{DisplaySource, Overlay};
use crate::repaint::Cadence;
use crate::theme;

use caudra_config::ClockFormat;
use caudra_grab::grab_scope;
use crossterm::event::{KeyEvent, MouseEvent};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

const TITLE: &str = " Message Actions ";
const MAX_VISIBLE: u16 = 6;
const WIDTH_PERCENT: u16 = 38;
/// Lines the details up with the action labels under them.
const DETAIL_INDENT: &str = "  ";
const DETAIL_SEPARATOR: &str = " · ";
const DATE_FORMAT: &str = "%Y-%m-%d ";
const THINKING_PREFIX: &str = "thinking ";
pub(crate) const ACTIONS_MAIN_CHAT_ONLY: &str = "Fork and revert work from the main chat";
pub(crate) const ACTIONS_AFTER_SAVE: &str = "Fork and revert open once this turn is saved";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageActionKind {
    Review,
    Fork,
    RevertBoth,
    RevertConversation,
    RevertFiles,
    Unrevert,
}

impl PickerItem for MessageActionKind {
    fn label(&self) -> &str {
        match self {
            Self::Review => "Review passages",
            Self::Fork => "Fork here",
            Self::RevertBoth => "Revert both",
            Self::RevertConversation => "Revert conversation",
            Self::RevertFiles => "Revert files",
            Self::Unrevert => "Unrevert",
        }
    }
}

pub enum MessageActionsAction {
    Consumed,
    Select {
        source: DisplaySource,
        kind: MessageActionKind,
    },
    Close,
    Copy(String),
}

/// What the menu can do at its message.
pub enum Offer<'a> {
    /// `files_withheld` says why the file reverts are left out; the title
    /// carries it so the menu explains their absence.
    Actions {
        source: DisplaySource,
        pending_revert: bool,
        files_withheld: Option<&'a str>,
    },
    /// Nothing here can act on the message, for the reason given.
    Nothing(&'static str),
}

/// When the message was made, with its date unless that is `now`'s, then the
/// usage of the request its card closed; under it, the model that wrote it
/// and at what level. A line with nothing known is left out.
pub(crate) fn details_lines(
    details: &MessageDetails,
    clock: ClockFormat,
    now: Timestamp,
) -> Option<Vec<Line<'static>>> {
    let theme = theme::current();
    let zone = TimeZone::system();
    let time = details.created.map(|created| {
        let zoned = created.to_zoned(zone.clone());
        let same_day = zoned.date() == now.to_zoned(zone.clone()).date();
        let format = crate::clock::hms(clock);
        let text = if same_day {
            zoned.strftime(format).to_string()
        } else {
            zoned
                .strftime(&format!("{DATE_FORMAT}{format}"))
                .to_string()
        };
        Span::styled(text, theme.timestamp)
    });
    let usage = details
        .usage
        .clone()
        .map(|usage| Span::styled(usage, theme.tool_dim));
    let model = details
        .producer
        .as_ref()
        .map(|producer| Span::styled(producer.model.clone(), theme.timestamp));
    let thinking = details
        .producer
        .as_ref()
        .and_then(|producer| producer.thinking.as_ref())
        .map(|level| Span::styled(format!("{THINKING_PREFIX}{level}"), theme.tool_dim));
    let lines: Vec<Line<'static>> = [detail_line([time, usage]), detail_line([model, thinking])]
        .into_iter()
        .flatten()
        .collect();
    (!lines.is_empty()).then_some(lines)
}

fn detail_line(parts: [Option<Span<'static>>; 2]) -> Option<Line<'static>> {
    let mut spans = vec![Span::raw(DETAIL_INDENT)];
    for part in parts.into_iter().flatten() {
        if spans.len() > 1 {
            spans.push(Span::raw(DETAIL_SEPARATOR));
        }
        spans.push(part);
    }
    (spans.len() > 1).then(|| Line::from(spans))
}

pub struct MessageActions {
    picker: ListPicker<MessageActionKind>,
    source: Option<DisplaySource>,
}

impl MessageActions {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new()
                .with_max_visible(MAX_VISIBLE)
                .with_width_percent(WIDTH_PERCENT),
            source: None,
        }
    }

    pub fn open(&mut self, offer: Offer, details: Option<Vec<Line<'static>>>) {
        self.picker.set_info_lines(details);
        let (source, pending_revert, files_withheld) = match offer {
            Offer::Actions {
                source,
                pending_revert,
                files_withheld,
            } => (source, pending_revert, files_withheld),
            Offer::Nothing(reason) => {
                self.source = None;
                self.picker.set_empty_text(reason);
                self.picker.open(Vec::new(), TITLE);
                return;
            }
        };
        let actions = [
            MessageActionKind::Review,
            MessageActionKind::Fork,
            MessageActionKind::RevertBoth,
            MessageActionKind::RevertConversation,
            MessageActionKind::RevertFiles,
            MessageActionKind::Unrevert,
        ]
        .into_iter()
        .filter(|kind| match kind {
            MessageActionKind::RevertBoth | MessageActionKind::RevertFiles => {
                files_withheld.is_none()
            }
            MessageActionKind::Unrevert => pending_revert,
            _ => true,
        })
        .collect();
        self.source = Some(source);
        self.picker.set_empty_text(NO_MATCHES);
        self.picker.open(
            actions,
            files_withheld.map_or_else(|| TITLE.to_owned(), |reason| format!("{TITLE}· {reason} ")),
        );
    }

    #[cfg(test)]
    pub(crate) fn offers(&self, kind: MessageActionKind) -> bool {
        (0..)
            .map_while(|index| self.picker.item(index))
            .any(|item| *item == kind)
    }

    #[cfg(test)]
    pub(crate) fn info_text(&self) -> Option<String> {
        self.picker.info_text()
    }

    #[cfg(test)]
    pub(crate) fn title(&self) -> &str {
        self.picker.title()
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.source = None;
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.handle_text(text, true)
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        self.picker.handle_text(text, paste)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MessageActionsAction {
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> MessageActionsAction {
        let action = self.picker.handle_mouse(event);
        self.map_picker_action(action)
    }

    fn map_picker_action(
        &mut self,
        action: PickerAction<MessageActionKind>,
    ) -> MessageActionsAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) | PickerAction::Key(_) => {
                MessageActionsAction::Consumed
            }
            PickerAction::Select(kind) => match self.source.take() {
                Some(source) => MessageActionsAction::Select { source, kind },
                None => MessageActionsAction::Close,
            },
            PickerAction::Close => {
                self.source = None;
                MessageActionsAction::Close
            }
            PickerAction::Copy(text) => MessageActionsAction::Copy(text),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("message_actions", area);
        self.picker.view(frame, area)
    }
}

impl Overlay for MessageActions {
    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(DocsTopic::MessageActions)
    }

    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }

    fn tooltip(&self) -> Option<Tip> {
        self.picker.tooltip()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::producer::Producer;
    use caudra_storage::id::CaudraId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use jiff::ToSpan;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn offer(
        source: DisplaySource,
        pending_revert: bool,
        files_withheld: Option<&str>,
    ) -> Offer<'_> {
        Offer::Actions {
            source,
            pending_revert,
            files_withheld,
        }
    }

    const UNREVERT_INDEX: usize = 5;
    const WITHHELD: &str = "No recorded file changes after this message";
    const USAGE: &str = "135.9k↑ 872↓ $0.055";
    const CREATED_MS: i64 = 1_790_000_000_000;
    const MODEL: &str = "anthropic/claude-opus-5-5";
    const LEVEL: &str = "high";
    const DAY_HOURS: i64 = 24;

    #[test]
    fn selection_preserves_source() {
        let source = DisplaySource::Reasoning(CaudraId::generate());
        let mut actions = MessageActions::new();
        actions.open(offer(source, false, None), None);

        let MessageActionsAction::Select {
            source: selected, ..
        } = actions.handle_key(enter())
        else {
            panic!("expected a selection");
        };
        assert_eq!(selected, source);
    }

    #[test]
    fn unrevert_is_only_present_for_pending_revert() {
        let source = DisplaySource::User(CaudraId::generate());
        let mut actions = MessageActions::new();
        actions.open(offer(source, false, None), None);
        assert_eq!(actions.picker.selected_index(), Some(0));
        assert!(actions.picker.item(UNREVERT_INDEX).is_none());

        actions.open(offer(source, true, None), None);
        assert_eq!(
            actions.picker.item(UNREVERT_INDEX),
            Some(&MessageActionKind::Unrevert)
        );
    }

    #[test_case(None, true ; "offered_without_a_reason")]
    #[test_case(Some(WITHHELD), false ; "withheld_with_the_reason_in_the_title")]
    fn file_reverts_follow_the_reason(withheld: Option<&str>, offered: bool) {
        let mut actions = MessageActions::new();
        actions.open(
            offer(DisplaySource::User(CaudraId::generate()), false, withheld),
            None,
        );
        for kind in [
            MessageActionKind::RevertBoth,
            MessageActionKind::RevertFiles,
        ] {
            assert_eq!(actions.offers(kind), offered, "{kind:?}");
        }
        assert!(actions.offers(MessageActionKind::RevertConversation));
        assert_eq!(actions.title().contains(WITHHELD), !offered);
    }

    fn created() -> Timestamp {
        Timestamp::from_millisecond(CREATED_MS).unwrap()
    }

    fn details(created: bool, usage: bool, producer: Option<Option<&str>>) -> MessageDetails {
        MessageDetails {
            created: created.then(self::created),
            producer: producer.map(|thinking| Producer {
                model: MODEL.into(),
                thinking: thinking.map(str::to_owned),
            }),
            usage: usage.then(|| USAGE.to_owned()),
        }
    }

    fn lines_text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test_case(true, true, None, &["{time} · {usage}"] ; "time_then_usage")]
    #[test_case(true, false, None, &["{time}"] ; "time_alone")]
    #[test_case(false, true, None, &["{usage}"] ; "usage_alone")]
    #[test_case(false, false, None, &[] ; "nothing_says_nothing")]
    #[test_case(true, true, Some(Some(LEVEL)), &["{time} · {usage}", "{model} · thinking {level}"] ; "model_and_level_under_the_time")]
    #[test_case(true, false, Some(None), &["{time}", "{model}"] ; "model_without_a_recorded_level")]
    #[test_case(false, false, Some(Some(LEVEL)), &["{model} · thinking {level}"] ; "model_alone")]
    fn the_details_read_time_and_usage_then_the_model(
        created: bool,
        usage: bool,
        producer: Option<Option<&str>>,
        expected: &[&str],
    ) {
        let now = self::created();
        let time = now
            .to_zoned(TimeZone::system())
            .strftime(crate::clock::hms(ClockFormat::Hour24))
            .to_string();
        let expected: Vec<String> = expected
            .iter()
            .map(|line| {
                let text = line
                    .replace("{time}", &time)
                    .replace("{usage}", USAGE)
                    .replace("{model}", MODEL)
                    .replace("{level}", LEVEL)
                    .replace(" · ", DETAIL_SEPARATOR);
                format!("{DETAIL_INDENT}{text}")
            })
            .collect();
        let lines = details_lines(&details(created, usage, producer), ClockFormat::Hour24, now);
        assert_eq!(
            lines.as_deref().map(lines_text).unwrap_or_default(),
            expected
        );
    }

    #[test_case(0, false ; "today_is_only_the_time")]
    #[test_case(2 * DAY_HOURS, true ; "another_day_names_its_date")]
    fn the_date_shows_only_when_it_is_not_today(hours_later: i64, dated: bool) {
        let now = created().checked_add(hours_later.hours()).unwrap();
        let lines = details_lines(&details(true, false, None), ClockFormat::Hour24, now).unwrap();
        let date = created()
            .to_zoned(TimeZone::system())
            .strftime("%Y-%m-%d")
            .to_string();
        assert_eq!(lines_text(&lines)[0].contains(&date), dated);
    }

    #[test]
    fn opening_without_details_clears_the_last_ones() {
        let mut actions = MessageActions::new();
        let source = DisplaySource::User(CaudraId::generate());
        let line = details_lines(&details(false, true, None), ClockFormat::Hour24, created());
        actions.open(offer(source, false, None), line);
        assert_eq!(
            actions.picker.info_text(),
            Some(format!("{DETAIL_INDENT}{USAGE}"))
        );

        actions.open(offer(source, false, None), None);
        assert_eq!(actions.picker.info_text(), None);
    }

    #[test]
    fn nothing_to_offer_opens_on_the_reason() {
        let mut actions = MessageActions::new();
        let line = details_lines(&details(false, true, None), ClockFormat::Hour24, created());
        actions.open(Offer::Nothing(ACTIONS_MAIN_CHAT_ONLY), line);

        assert!(actions.is_open());
        assert!(actions.picker.item(0).is_none());
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                actions.view(frame, frame.area());
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains(ACTIONS_MAIN_CHAT_ONLY), "{screen}");
        assert!(screen.contains(USAGE), "{screen}");
        assert!(matches!(
            actions.handle_key(enter()),
            MessageActionsAction::Close | MessageActionsAction::Consumed
        ));
    }
}
