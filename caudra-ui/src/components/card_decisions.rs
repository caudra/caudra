//! What the decision engine decided, or is deciding, drawn as chips on the
//! row it belongs to. A call's sit on the call's card, with a section at the
//! top of its opened body. A turn's sit on the row the transcript ended on
//! when the engine was asked, and a click on them opens the same details
//! under that row.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::decisions::{
    DecisionFeature, NoteAnchor, NoteOutcome, PendingPhase, ScreeningVerdict,
};
use caudra_agent::{format_live_duration, format_settled_duration};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::tool_display::{DECISION_BODY_INDENT, DECISION_PREFIX, RoleStyle, decision_style};
use super::{DECISION_SEPARATOR, SHADOW_MARKER};
use crate::animation::{live_elapsed, spinner_str};
use crate::theme;

const CHIP_GAP: &str = " ";
const CHIP_SEPARATOR: &str = " · ";
const PENDING_SUFFIX: &str = "…";
/// Between a decision's words and how long the engine took over it.
const CLOCK_GAP: &str = " ";
/// Past this many chips a header says how many there are instead.
const MAX_CHIPS: usize = 2;
const ONE_DECISION: &str = "decision";
const MANY_DECISIONS: &str = "decisions";
/// What a row says when every decision on it came to nothing.
const ALL_QUIET: &str = "checked";
const CHILD_SEPARATOR: char = ':';
const QUEUED: &str = "waiting to screen";
const RETRYING: &str = "retrying screening";

/// What a decision is about: a call, drawn on the card that draws the call,
/// or the turn, drawn on the row the transcript ended on when it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionSubject {
    Call(String),
    Turn,
}

impl From<&NoteAnchor> for DecisionSubject {
    fn from(anchor: &NoteAnchor) -> Self {
        match anchor {
            NoteAnchor::Tool(call_id) => Self::Call(call_id.clone()),
            NoteAnchor::History(_) => Self::Turn,
        }
    }
}

/// One decision on a row. A call's subject is the call it is about, which
/// for a batch child is the child's id rather than the card's.
#[derive(Debug, Clone, PartialEq)]
pub struct CardDecision {
    pub subject: DecisionSubject,
    pub feature: DecisionFeature,
    pub state: DecisionState,
}

/// The decisions about a batch's children, by index in its roster.
pub type ChildDecisions = Arc<HashMap<usize, Vec<CardDecision>>>;

#[derive(Debug, Clone, PartialEq)]
pub enum DecisionState {
    /// The engine has not answered yet. `phase` says what holds it up,
    /// when it is more than the engine thinking.
    Pending {
        since: Instant,
        phase: Option<PendingPhase>,
    },
    /// `text` is what a standalone row would hold: its title, then a detail
    /// per line. `took` is how long the wait it answered ran, unknown for one
    /// that arrived without a wait and for a restored one. `screening` is
    /// what content screening concluded, unknown for a row stored before
    /// screening said.
    Settled {
        outcome: NoteOutcome,
        text: String,
        took: Option<Duration>,
        screening: Option<ScreeningVerdict>,
    },
    /// A turn's wait the engine answered with nothing to do. `text` is titled
    /// by the feature's quiet words, then lists what came closest, one per
    /// line. `took` is how long the wait ran, unknown when the feature had
    /// nothing to ask and so never waited. Live only: nothing about it is
    /// stored.
    Quiet {
        text: String,
        took: Option<Duration>,
    },
}

/// A chip as drawn: its lead glyph and words in its tone, then its clock.
struct Chip {
    lead: &'static str,
    words: String,
    clock: Option<String>,
    tone: Tone,
}

impl CardDecision {
    pub fn pending(subject: DecisionSubject, feature: DecisionFeature) -> Self {
        Self {
            subject,
            feature,
            state: DecisionState::Pending {
                since: Instant::now(),
                phase: None,
            },
        }
    }

    /// A wait held up by `phase`.
    pub fn in_phase(mut self, phase: Option<PendingPhase>) -> Self {
        if let DecisionState::Pending { phase: held, .. } = &mut self.state {
            *held = phase;
        }
        self
    }

    /// An answer that says what content screening concluded.
    pub fn screened(mut self, verdict: Option<ScreeningVerdict>) -> Self {
        if let DecisionState::Settled { screening, .. } = &mut self.state {
            *screening = verdict;
        }
        self
    }

    pub fn settled(
        subject: DecisionSubject,
        feature: DecisionFeature,
        outcome: NoteOutcome,
        summary: &str,
        details: &[String],
    ) -> Self {
        let text = decision_text(feature.label(), outcome, summary, details);
        Self {
            subject,
            feature,
            state: DecisionState::Settled {
                outcome,
                text,
                took: None,
                screening: None,
            },
        }
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.state, DecisionState::Pending { .. })
    }

    fn is_quiet(&self) -> bool {
        matches!(self.state, DecisionState::Quiet { .. })
    }

    pub fn is_pending_for(&self, subject: &DecisionSubject, feature: &DecisionFeature) -> bool {
        self.is_pending() && self.subject == *subject && self.feature == *feature
    }

    fn waited(&self) -> Option<Duration> {
        match self.state {
            DecisionState::Pending { since, .. } => Some(live_elapsed(since)),
            DecisionState::Settled { .. } | DecisionState::Quiet { .. } => None,
        }
    }

    fn quiet_took(&self) -> Option<Duration> {
        match self.state {
            DecisionState::Quiet { took, .. } => took,
            DecisionState::Pending { .. } | DecisionState::Settled { .. } => None,
        }
    }

    fn chip(&self) -> Chip {
        let words = chip_words(&self.feature);
        let (lead, said, clock, tone) = match &self.state {
            DecisionState::Pending { since, phase } => {
                let waited = live_elapsed(*since);
                let said = match phase {
                    Some(PendingPhase::Queued) => QUEUED,
                    Some(PendingPhase::Retrying) => RETRYING,
                    None => words.pending,
                };
                (
                    spinner_str(waited.as_millis()),
                    format!("{said}{PENDING_SUFFIX}"),
                    Some(format_live_duration(waited)),
                    Tone::Pending,
                )
            }
            DecisionState::Settled {
                outcome,
                took,
                screening,
                ..
            } => {
                let (said, tone) = match (outcome, screening) {
                    (NoteOutcome::Shadow, _) => (words.shadow, Tone::Shadow),
                    (NoteOutcome::Acted | NoteOutcome::FellBack, Some(verdict)) => {
                        (verdict.label(), Tone::Loud)
                    }
                    (NoteOutcome::Acted, None) if words.loud => (words.acted, Tone::Loud),
                    (NoteOutcome::Acted, None) => (words.acted, Tone::Acted),
                    (NoteOutcome::FellBack, None) => (words.failed, Tone::Loud),
                };
                (
                    DECISION_PREFIX,
                    said.to_owned(),
                    took.map(format_settled_duration),
                    tone,
                )
            }
            DecisionState::Quiet { took, .. } => (
                DECISION_PREFIX,
                words.quiet.to_owned(),
                took.map(format_settled_duration),
                Tone::Quiet,
            ),
        };
        Chip {
            lead,
            words: said,
            clock,
            tone,
        }
    }
}

/// Chips as drawn, and the text they copy and search as: each one behind the
/// settled prefix and without its clock, since neither a spinner frame nor a
/// number that changes every frame is anything to search for.
#[derive(Default)]
pub struct Chips {
    pub spans: Vec<Span<'static>>,
    pub text: String,
}

/// Puts `decision` in the pending slot it answers, timed by that wait, or
/// after the rest.
pub fn record(decisions: &mut Vec<CardDecision>, mut decision: CardDecision) {
    match decisions
        .iter_mut()
        .find(|held| held.is_pending_for(&decision.subject, &decision.feature))
    {
        Some(slot) => {
            match (&mut decision.state, &slot.state) {
                (DecisionState::Settled { took, .. }, _) => *took = slot.waited(),
                (
                    DecisionState::Pending { since, .. },
                    DecisionState::Pending { since: held, .. },
                ) => {
                    *since = *held;
                }
                _ => {}
            }
            *slot = decision;
        }
        None => decisions.push(decision),
    }
}

/// Drops the wait for `feature` on `subject`, saying whether there was one.
pub fn settle(
    decisions: &mut Vec<CardDecision>,
    subject: &DecisionSubject,
    feature: &DecisionFeature,
) -> bool {
    let before = decisions.len();
    decisions.retain(|held| !held.is_pending_for(subject, feature));
    decisions.len() != before
}

/// Answers the wait for `feature` on `subject` with nothing to do. Unlike a
/// call's own, it stays, timed by the wait and holding the `near_misses`, so
/// the row it sits on keeps its height and a click still says what was
/// weighed. `cause` names what started it.
pub fn quiet(
    decisions: &mut [CardDecision],
    subject: &DecisionSubject,
    feature: &DecisionFeature,
    cause: Option<&str>,
    near_misses: &[String],
) -> bool {
    for held in decisions.iter_mut() {
        if let DecisionState::Pending { since, .. } = held.state
            && held.subject == *subject
            && held.feature == *feature
        {
            held.state = DecisionState::Quiet {
                text: quiet_text(feature, cause, near_misses),
                took: Some(live_elapsed(since)),
            };
            return true;
        }
    }
    false
}

/// Records that `feature` had nothing to ask the engine about `subject`,
/// `reasons` saying why: a quiet answer with no wait, so no time. A row that
/// already says what the feature did keeps that.
pub fn skipped(
    decisions: &mut Vec<CardDecision>,
    subject: DecisionSubject,
    feature: DecisionFeature,
    cause: Option<&str>,
    reasons: &[String],
) {
    if decisions
        .iter()
        .any(|held| held.subject == subject && held.feature == feature)
    {
        return;
    }
    decisions.push(CardDecision {
        state: DecisionState::Quiet {
            text: quiet_text(&feature, cause, reasons),
            took: None,
        },
        subject,
        feature,
    });
}

/// Whether a wait that ends with nothing to do leaves a quiet chip. A turn's
/// does, wherever it sits; a call's own goes, since its card says the rest.
pub fn keeps_quiet(subject: &DecisionSubject, feature: &DecisionFeature) -> bool {
    *subject == DecisionSubject::Turn
        || matches!(
            feature,
            DecisionFeature::MemoryRecall
                | DecisionFeature::SkillSuggestions
                | DecisionFeature::GoalPrescreen
                | DecisionFeature::QuestionToolNudge
                | DecisionFeature::CompactionRetention
        )
}

fn quiet_text(feature: &DecisionFeature, cause: Option<&str>, details: &[String]) -> String {
    let words = chip_words(feature).quiet;
    let summary = match cause {
        Some(cause) => format!("{cause}{DECISION_SEPARATOR}{words}"),
        None => words.to_owned(),
    };
    decision_text(feature.label(), NoteOutcome::Acted, &summary, details)
}

/// Drops every wait about a call once the card's call is over. A turn's wait
/// on the card is the turn's to answer.
pub fn settle_all(decisions: &mut Vec<CardDecision>) -> bool {
    let before = decisions.len();
    decisions.retain(|held| !held.is_pending() || held.subject == DecisionSubject::Turn);
    decisions.len() != before
}

/// The decisions a card's header shows: those about `call_id` itself, not its
/// children, and the turn's that landed on it.
pub fn about<'a>(
    decisions: &'a [CardDecision],
    call_id: &'a str,
) -> impl Iterator<Item = &'a CardDecision> {
    decisions
        .iter()
        .filter(move |decision| match &decision.subject {
            DecisionSubject::Call(subject) => subject == call_id,
            DecisionSubject::Turn => true,
        })
}

/// A chip for each decision, led by a gap, or nothing. Quiet ones stay out,
/// past [`MAX_CHIPS`] the rest are only counted, and when every one is quiet
/// a single dim chip says so.
pub fn chip_spans<'a>(decisions: impl IntoIterator<Item = &'a CardDecision>) -> Chips {
    let decisions: Vec<&CardDecision> = decisions.into_iter().collect();
    drawn_chips(&decisions, false, CHIP_GAP)
}

/// A row's chips as they come, then counted, with nothing in front: the row
/// pads them out to its edge itself.
pub fn row_chips(decisions: &[CardDecision]) -> [Chips; 2] {
    let decisions: Vec<&CardDecision> = decisions.iter().collect();
    [false, true].map(|counted| drawn_chips(&decisions, counted, ""))
}

/// The chips [`chip_spans`] describes, `counted` however few there are. A
/// count is timed by the longest wait still running.
fn drawn_chips(decisions: &[&CardDecision], counted: bool, lead: &'static str) -> Chips {
    let mut chips = Chips::default();
    let noted: Vec<&CardDecision> = decisions
        .iter()
        .copied()
        .filter(|decision| !decision.is_quiet())
        .collect();
    if noted.is_empty() {
        if decisions.iter().any(|decision| decision.is_quiet()) {
            let longest = decisions
                .iter()
                .filter_map(|decision| decision.quiet_took())
                .max();
            chips.push(
                Span::raw(lead),
                Chip {
                    lead: DECISION_PREFIX,
                    words: ALL_QUIET.to_owned(),
                    clock: longest.map(format_settled_duration),
                    tone: Tone::Quiet,
                },
            );
        }
        return chips;
    }
    if counted || noted.len() > MAX_CHIPS {
        let tone = noted
            .iter()
            .map(|decision| decision.chip().tone)
            .max()
            .unwrap_or(Tone::Acted);
        let longest = noted
            .iter()
            .filter_map(|decision| decision.waited())
            .max()
            .map(format_live_duration);
        let noun = match noted.len() {
            1 => ONE_DECISION,
            _ => MANY_DECISIONS,
        };
        chips.push(
            Span::raw(lead),
            Chip {
                lead: DECISION_PREFIX,
                words: format!("{} {noun}", noted.len()),
                clock: longest,
                tone,
            },
        );
        return chips;
    }
    for (index, decision) in noted.into_iter().enumerate() {
        let separator = match index {
            0 => Span::raw(lead),
            _ => Span::styled(CHIP_SEPARATOR, theme::current().tool_dim),
        };
        chips.push(separator, decision.chip());
    }
    chips
}

impl Chips {
    fn push(&mut self, separator: Span<'static>, chip: Chip) {
        self.text.push_str(&separator.content);
        self.text.push_str(DECISION_PREFIX);
        self.text.push_str(&chip.words);
        self.spans.push(separator);
        self.spans.push(Span::styled(
            format!("{}{}", chip.lead, chip.words),
            chip.tone.style(),
        ));
        if let Some(clock) = chip.clock {
            self.spans.push(Span::styled(
                format!("{CLOCK_GAP}{clock}"),
                theme::current().tool_dim,
            ));
        }
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }
}

/// The decisions about the children of the batch `root`, by index in its
/// roster.
pub fn children(decisions: &[CardDecision], root: &str) -> ChildDecisions {
    let mut children: HashMap<usize, Vec<CardDecision>> = HashMap::new();
    for decision in decisions {
        if let DecisionSubject::Call(call_id) = &decision.subject
            && let Some(index) = call_id
                .strip_prefix(root)
                .and_then(|rest| rest.strip_prefix(CHILD_SEPARATOR))
                .and_then(|index| index.parse().ok())
        {
            children.entry(index).or_default().push(decision.clone());
        }
    }
    Arc::new(children)
}

/// Every answered decision on the card as an open row draws it, with the text
/// each one copies and searches as.
pub fn section<'a>(
    decisions: impl IntoIterator<Item = &'a CardDecision>,
) -> Vec<(Vec<Line<'static>>, &'a str)> {
    answered(decisions, false)
}

/// What a click on a row's chips opens under it: its [`section`], each title
/// timed by how long the engine took.
pub fn details(decisions: &[CardDecision]) -> Vec<(Vec<Line<'static>>, &str)> {
    answered(decisions, true)
}

fn answered<'a>(
    decisions: impl IntoIterator<Item = &'a CardDecision>,
    timed: bool,
) -> Vec<(Vec<Line<'static>>, &'a str)> {
    decisions
        .into_iter()
        .filter_map(|decision| match &decision.state {
            DecisionState::Settled {
                outcome,
                text,
                took,
                ..
            } => Some((
                decision_lines(text, *outcome, took.filter(|_| timed), true),
                text.as_str(),
            )),
            DecisionState::Quiet { text, took } => Some((
                styled_lines(text, quiet_style(), took.filter(|_| timed), true),
                text.as_str(),
            )),
            DecisionState::Pending { .. } => None,
        })
        .collect()
}

/// A quiet answer reads as dim as one that only says what it would have done.
fn quiet_style() -> RoleStyle {
    decision_style(NoteOutcome::Shadow)
}

/// A decision's title behind its prefix, then how long the engine `took`
/// when known, and once `open`, each detail under it.
pub fn decision_lines(
    text: &str,
    outcome: NoteOutcome,
    took: Option<Duration>,
    open: bool,
) -> Vec<Line<'static>> {
    styled_lines(text, decision_style(outcome), took, open)
}

fn styled_lines(
    text: &str,
    style: RoleStyle,
    took: Option<Duration>,
    open: bool,
) -> Vec<Line<'static>> {
    let mut lines = text.lines();
    let mut title = vec![
        Span::styled(style.prefix, style.prefix_style),
        Span::styled(
            lines.next().unwrap_or_default().to_owned(),
            style.text_style,
        ),
    ];
    if let Some(took) = took {
        title.push(Span::styled(
            format!("{DECISION_SEPARATOR}{}", format_settled_duration(took)),
            theme::current().tool_dim,
        ));
    }
    let mut drawn = vec![Line::from(title)];
    if open {
        drawn.extend(lines.map(|detail| {
            Line::from(vec![
                Span::raw(DECISION_BODY_INDENT),
                Span::styled(detail.to_owned(), style.text_style),
            ])
        }));
    }
    drawn
}

/// The title names `feature`, as its config key reads, and what it did; each
/// detail is a line under it.
pub fn decision_text(
    feature: &str,
    outcome: NoteOutcome,
    summary: &str,
    details: &[String],
) -> String {
    let marker = match outcome {
        NoteOutcome::Shadow => SHADOW_MARKER,
        NoteOutcome::Acted | NoteOutcome::FellBack => "",
    };
    let mut text = format!("{marker}{feature}{DECISION_SEPARATOR}{summary}");
    for detail in details {
        text.push('\n');
        text.push_str(detail);
    }
    text
}

/// How loud a chip is, quietest first, so the loudest present colours a
/// header that only counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tone {
    Quiet,
    Shadow,
    Pending,
    Acted,
    Loud,
}

impl Tone {
    fn style(self) -> Style {
        let theme = theme::current();
        match self {
            Self::Quiet | Self::Shadow => theme.status_dim.add_modifier(Modifier::DIM),
            Self::Pending => theme.spinner,
            Self::Acted => theme.accent,
            Self::Loud => theme.tool_warning,
        }
    }
}

/// What a feature's chip says while it waits, once it acted, in shadow, when
/// the engine failed, and when it found nothing to do. `loud` features act by
/// warning the reader, so an acted chip warns too.
struct ChipWords {
    pending: &'static str,
    acted: &'static str,
    shadow: &'static str,
    failed: &'static str,
    quiet: &'static str,
    loud: bool,
}

const fn words(
    pending: &'static str,
    acted: &'static str,
    shadow: &'static str,
    failed: &'static str,
    quiet: &'static str,
) -> ChipWords {
    ChipWords {
        pending,
        acted,
        shadow,
        failed,
        quiet,
        loud: false,
    }
}

const fn loud(words: ChipWords) -> ChipWords {
    ChipWords {
        loud: true,
        ..words
    }
}

const RANKING: ChipWords = words(
    "ranking",
    "ranked",
    "would rank",
    "ranking failed",
    "kept the order",
);
const DECIDING: ChipWords = words(
    "deciding",
    "decided",
    "would decide",
    "decision failed",
    "no change",
);

fn chip_words(feature: &DecisionFeature) -> ChipWords {
    match feature {
        DecisionFeature::SourceCodeContext => words(
            "reranking",
            "reranked",
            "would rerank",
            "rerank failed",
            "kept the order",
        ),
        DecisionFeature::MemorySearch | DecisionFeature::HistorySearch => RANKING,
        DecisionFeature::MemoryDuplicates => words(
            "checking duplicates",
            "may duplicate",
            "would flag duplicates",
            "duplicate check failed",
            "no duplicates",
        ),
        DecisionFeature::AutoScreening => loud(words(
            "screening",
            "asked you",
            "would ask",
            "screening failed",
            "no concerns",
        )),
        DecisionFeature::ShellEffect => loud(words(
            "checking writes",
            "may write",
            "would warn",
            "write check failed",
            "no writes",
        )),
        DecisionFeature::ContentScreening => words(
            "screening result",
            "screening result",
            "would flag",
            "screening failed",
            "nothing flagged",
        ),
        DecisionFeature::OutputPointers => words(
            "finding omitted lines",
            "pointed",
            "would point",
            "pointing failed",
            "no pointers",
        ),
        DecisionFeature::SubagentRouting => words(
            "routing",
            "routed",
            "would route",
            "routing failed",
            "kept the route",
        ),
        DecisionFeature::MemoryRecall => words(
            "recalling notes",
            "recalled",
            "would recall",
            "recall failed",
            "no notes",
        ),
        DecisionFeature::SkillSuggestions => deciding("matching skills", "no skill"),
        DecisionFeature::GoalPrescreen => deciding("checking goal", "goal checked"),
        DecisionFeature::QuestionToolNudge => deciding("checking question", "no nudge"),
        DecisionFeature::CompactionRetention => deciding("picking results to keep", "kept as is"),
        DecisionFeature::Workflow | DecisionFeature::Code | DecisionFeature::PermissionAdvice => {
            DECIDING
        }
    }
}

/// [`DECIDING`] with words of its own for the wait and for a quiet answer.
const fn deciding(pending: &'static str, quiet: &'static str) -> ChipWords {
    ChipWords {
        pending,
        quiet,
        ..DECIDING
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use caudra_agent::decisions::{DecisionFeature, NoteOutcome, PendingPhase, ScreeningVerdict};
    use ratatui::text::Span;
    use test_case::test_case;

    use super::{
        ALL_QUIET, CardDecision, DecisionState, DecisionSubject, MANY_DECISIONS, ONE_DECISION,
        Tone, about, children, chip_spans, details, keeps_quiet, quiet, record, row_chips, section,
        settle, settle_all, skipped,
    };
    use crate::animation::test_clock::{FrozenClock, FrozenSpinner};
    use crate::components::DECISION_SEPARATOR;
    use crate::components::tool_display::DECISION_PREFIX;

    const CALL: &str = "t1";
    const CHILD: &str = "t1:2";
    const GRANDCHILD: &str = "t1:2:0";
    const SUMMARY: &str = "moved 5 of 12 rows up";
    const DETAIL: &str = "0.97  src/lib.rs:12 chip  moved 4th to 1st";
    const NEAR_MISS: &str = "0.74  12+1 render-loop.md  below 0.80";
    const WAITED: Duration = Duration::from_millis(1_234);
    const WAITED_TEXT: &str = "1.2s";
    const FIRST_FRAME: &str = "⠋ ";
    const LONG_AGO: Duration = Duration::from_secs(61);
    const LONG_AGO_PREFIX: &str = " 1m 1.";
    const RECENTLY: Duration = Duration::from_secs(1);
    const NO_NOTES: &str = "no notes";
    const CAUSE: &str = "step started";
    const NO_SKILL: &str = "no skill";

    fn call(call_id: &str) -> DecisionSubject {
        DecisionSubject::Call(call_id.into())
    }

    fn settled(
        subject: DecisionSubject,
        feature: DecisionFeature,
        outcome: NoteOutcome,
    ) -> CardDecision {
        CardDecision::settled(subject, feature, outcome, SUMMARY, &[DETAIL.into()])
    }

    fn timed(decision: CardDecision, waited: Duration) -> CardDecision {
        let DecisionState::Settled {
            outcome,
            text,
            screening,
            ..
        } = decision.state
        else {
            unreachable!("only a settled decision is timed")
        };
        CardDecision {
            state: DecisionState::Settled {
                outcome,
                text,
                took: Some(waited),
                screening,
            },
            ..decision
        }
    }

    fn pending(subject: DecisionSubject, feature: DecisionFeature) -> CardDecision {
        CardDecision::pending(subject, feature)
    }

    fn waiting_for(
        subject: DecisionSubject,
        feature: DecisionFeature,
        waited: Duration,
    ) -> CardDecision {
        CardDecision {
            subject,
            feature,
            state: DecisionState::Pending {
                since: Instant::now() - waited,
                phase: None,
            },
        }
    }

    /// A turn's wait for `feature` the engine answered with nothing to do.
    fn quieted(feature: DecisionFeature) -> CardDecision {
        let mut decisions = vec![pending(DecisionSubject::Turn, feature.clone())];
        assert!(quiet(
            &mut decisions,
            &DecisionSubject::Turn,
            &feature,
            None,
            &[NEAR_MISS.into()]
        ));
        decisions.remove(0)
    }

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test_case(settled(call(CALL), DecisionFeature::SourceCodeContext, NoteOutcome::Acted), "reranked", Tone::Acted ; "acted")]
    #[test_case(settled(call(CALL), DecisionFeature::MemorySearch, NoteOutcome::Shadow), "would rank", Tone::Shadow ; "shadow")]
    #[test_case(settled(call(CALL), DecisionFeature::OutputPointers, NoteOutcome::FellBack), "pointing failed", Tone::Loud ; "a_fallback_warns")]
    #[test_case(settled(call(CALL), DecisionFeature::AutoScreening, NoteOutcome::Acted), "asked you", Tone::Loud ; "a_screening_that_acted_warns")]
    #[test_case(settled(call(CALL), DecisionFeature::ContentScreening, NoteOutcome::Shadow), "would flag", Tone::Shadow ; "a_shadow_screening_stays_quiet")]
    #[test_case(settled(DecisionSubject::Turn, DecisionFeature::MemoryRecall, NoteOutcome::Acted), "recalled", Tone::Acted ; "a_recall")]
    fn a_chip_says_what_the_feature_did(decision: CardDecision, word: &str, tone: Tone) {
        let chip = decision.chip();
        assert_eq!((chip.words.as_str(), chip.tone), (word, tone));
        let chips = chip_spans([&decision]);
        assert_eq!(text(&chips.spans), format!(" {DECISION_PREFIX}{word}"));
        assert_eq!(chips.text, format!(" {DECISION_PREFIX}{word}"));
    }

    #[test_case(NoteOutcome::Acted, Some(ScreeningVerdict::Flagged), "flagged", Tone::Loud ; "a_flag")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::TimedOut), "screening timed out", Tone::Loud ; "timed_out")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::Unreachable), "engine unreachable", Tone::Loud ; "unreachable")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::Busy), "screening busy", Tone::Loud ; "busy")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::OutOfTime), "screening incomplete", Tone::Loud ; "out_of_time")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::Failed), "screening failed", Tone::Loud ; "failed")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::Partial), "screening incomplete", Tone::Loud ; "partial")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::TooLarge), "screening size limit", Tone::Loud ; "too_large")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::Interrupted), "screening interrupted", Tone::Loud ; "interrupted")]
    #[test_case(NoteOutcome::FellBack, Some(ScreeningVerdict::Unreadable), "screening read failed", Tone::Loud ; "unreadable")]
    #[test_case(NoteOutcome::Acted, None, "screening result", Tone::Acted ; "a_stored_row_never_reads_as_a_flag")]
    #[test_case(NoteOutcome::FellBack, None, "screening failed", Tone::Loud ; "a_stored_failure")]
    fn a_content_chip_says_what_screening_concluded(
        outcome: NoteOutcome,
        verdict: Option<ScreeningVerdict>,
        word: &str,
        tone: Tone,
    ) {
        let decision =
            settled(call(CALL), DecisionFeature::ContentScreening, outcome).screened(verdict);

        let chip = decision.chip();

        assert_eq!((chip.words.as_str(), chip.tone), (word, tone));
        assert!(
            verdict == Some(ScreeningVerdict::Flagged) || !chip.words.contains("flag"),
            "{word}"
        );
    }

    #[test_case(None, "screening result…" ; "asking")]
    #[test_case(Some(PendingPhase::Queued), "waiting to screen…" ; "queued")]
    #[test_case(Some(PendingPhase::Retrying), "retrying screening…" ; "retrying")]
    fn a_screening_wait_says_what_holds_it_up_and_keeps_its_start(
        phase: Option<PendingPhase>,
        word: &str,
    ) {
        let _clock = FrozenClock::at(WAITED);
        let _spinner = FrozenSpinner::at(0);
        let feature = DecisionFeature::ContentScreening;
        let waiting = waiting_for(call(CALL), feature.clone(), LONG_AGO);
        let DecisionState::Pending { since: started, .. } = waiting.state else {
            unreachable!("a wait is pending")
        };
        let mut decisions = vec![waiting];

        record(&mut decisions, pending(call(CALL), feature).in_phase(phase));

        assert_eq!(decisions.len(), 1);
        assert!(
            matches!(decisions[0].state, DecisionState::Pending { since, phase: held } if since == started && held == phase)
        );
        assert_eq!(
            text(&chip_spans(&decisions).spans),
            format!(" {FIRST_FRAME}{word} {WAITED_TEXT}")
        );
    }

    #[test_case(DecisionFeature::SourceCodeContext, "reranking…" ; "a_rerank")]
    #[test_case(DecisionFeature::MemoryRecall, "recalling notes…" ; "a_recall")]
    #[test_case(DecisionFeature::CompactionRetention, "picking results to keep…" ; "a_retention")]
    fn a_wait_spins_and_counts_but_searches_as_its_words(feature: DecisionFeature, word: &str) {
        let _clock = FrozenClock::at(WAITED);
        let _spinner = FrozenSpinner::at(0);
        let decision = pending(call(CALL), feature);

        let chips = chip_spans([&decision]);

        assert_eq!(decision.chip().tone, Tone::Pending);
        assert_eq!(
            text(&chips.spans),
            format!(" {FIRST_FRAME}{word} {WAITED_TEXT}")
        );
        assert_eq!(chips.text, format!(" {DECISION_PREFIX}{word}"));
    }

    #[test]
    fn an_answer_to_a_wait_says_how_long_it_took() {
        let _clock = FrozenClock::at(WAITED);
        let feature = DecisionFeature::SourceCodeContext;
        let mut decisions = vec![pending(call(CALL), feature.clone())];

        record(
            &mut decisions,
            settled(call(CALL), feature, NoteOutcome::Acted),
        );

        let chips = chip_spans(&decisions);
        assert_eq!(
            text(&chips.spans),
            format!(" {DECISION_PREFIX}reranked {WAITED_TEXT}")
        );
        assert_eq!(chips.text, format!(" {DECISION_PREFIX}reranked"));
    }

    #[test]
    fn two_chips_join_and_more_are_counted_in_the_loudest_tone() {
        let mut decisions = vec![
            settled(
                call(CALL),
                DecisionFeature::OutputPointers,
                NoteOutcome::Acted,
            ),
            settled(
                call(CALL),
                DecisionFeature::AutoScreening,
                NoteOutcome::Acted,
            ),
        ];
        assert_eq!(
            text(&chip_spans(&decisions).spans),
            format!(" {DECISION_PREFIX}pointed · {DECISION_PREFIX}asked you")
        );

        decisions.push(settled(
            call(CALL),
            DecisionFeature::ShellEffect,
            NoteOutcome::Shadow,
        ));
        let chips = chip_spans(&decisions);
        assert_eq!(
            text(&chips.spans),
            format!(" {DECISION_PREFIX}3 {MANY_DECISIONS}")
        );
        assert_eq!(chips.spans[1].style, Tone::Loud.style());
    }

    #[test]
    fn a_counted_header_is_timed_by_its_longest_wait() {
        let decisions = [
            settled(
                call(CALL),
                DecisionFeature::OutputPointers,
                NoteOutcome::Acted,
            ),
            waiting_for(call(CALL), DecisionFeature::ContentScreening, RECENTLY),
            waiting_for(call(CALL), DecisionFeature::ShellEffect, LONG_AGO),
        ];

        let chips = chip_spans(&decisions);

        let [_, count, clock] = chips.spans.as_slice() else {
            panic!("a counted header is a gap, the count, and a clock")
        };
        assert_eq!(
            count.content,
            format!("{DECISION_PREFIX}3 {MANY_DECISIONS}")
        );
        assert!(
            clock.content.starts_with(LONG_AGO_PREFIX),
            "{}",
            clock.content
        );
        assert_eq!(chips.text, format!(" {DECISION_PREFIX}3 {MANY_DECISIONS}"));
    }

    #[test]
    fn an_answer_takes_its_waits_place_and_a_calls_quiet_one_drops_it() {
        let _clock = FrozenClock::at(WAITED);
        let feature = DecisionFeature::MemorySearch;
        let other = pending(call(CALL), DecisionFeature::OutputPointers);
        let mut decisions = vec![pending(call(CALL), feature.clone()), other.clone()];

        record(
            &mut decisions,
            settled(call(CALL), feature.clone(), NoteOutcome::Acted),
        );
        let answered = timed(
            settled(call(CALL), feature.clone(), NoteOutcome::Acted),
            WAITED,
        );
        assert_eq!(decisions, [answered.clone(), other]);

        assert!(!settle(&mut decisions, &call(CALL), &feature));
        assert!(settle(
            &mut decisions,
            &call(CALL),
            &DecisionFeature::OutputPointers
        ));
        assert_eq!(decisions, [answered]);
    }

    #[test]
    fn an_answer_without_a_wait_is_untimed() {
        let mut decisions = Vec::new();
        let decision = settled(
            call(CALL),
            DecisionFeature::MemorySearch,
            NoteOutcome::Acted,
        );

        record(&mut decisions, decision.clone());

        assert_eq!(decisions, [decision]);
    }

    #[test]
    fn the_call_ending_drops_its_waits_but_keeps_the_turns_and_what_was_decided() {
        let kept = settled(
            call(CHILD),
            DecisionFeature::MemorySearch,
            NoteOutcome::Acted,
        );
        let turn = pending(DecisionSubject::Turn, DecisionFeature::MemoryRecall);
        let mut decisions = vec![
            pending(call(CALL), DecisionFeature::OutputPointers),
            kept.clone(),
            turn.clone(),
        ];

        assert!(settle_all(&mut decisions));
        assert_eq!(decisions, [kept, turn]);
    }

    #[test]
    fn a_batch_hands_each_direct_child_its_own_decisions_and_its_header_the_turns() {
        let mine = settled(
            call(CALL),
            DecisionFeature::SubagentRouting,
            NoteOutcome::Acted,
        );
        let child = settled(
            call(CHILD),
            DecisionFeature::MemorySearch,
            NoteOutcome::Acted,
        );
        let grandchild = settled(
            call(GRANDCHILD),
            DecisionFeature::MemorySearch,
            NoteOutcome::Acted,
        );
        let turn = settled(
            DecisionSubject::Turn,
            DecisionFeature::MemoryRecall,
            NoteOutcome::Acted,
        );
        let decisions = [mine.clone(), child.clone(), grandchild, turn.clone()];

        assert_eq!(about(&decisions, CALL).collect::<Vec<_>>(), [&mine, &turn]);
        let children = children(&decisions, CALL);
        assert_eq!(children.len(), 1);
        assert_eq!(children[&2], [child]);
    }

    #[test]
    fn only_an_answered_decision_has_a_section() {
        let waiting = [pending(call(CALL), DecisionFeature::SourceCodeContext)];
        assert!(section(&waiting).is_empty());

        let decided = [settled(
            call(CALL),
            DecisionFeature::SourceCodeContext,
            NoteOutcome::Acted,
        )];
        let [(lines, text)] = section(&decided).try_into().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with(&format!("{SUMMARY}\n{DETAIL}")));
    }

    /// Most turn waits find nothing. The chip stays, dim and timed, so the
    /// row it sits on keeps its height, and its details keep what came
    /// closest.
    #[test]
    fn a_turns_quiet_answer_stays_dim_with_its_near_misses() {
        let _clock = FrozenClock::at(WAITED);
        let decisions = [quieted(DecisionFeature::MemoryRecall)];

        let chips = chip_spans(&decisions);
        assert_eq!(
            text(&chips.spans),
            format!(" {DECISION_PREFIX}{ALL_QUIET} {WAITED_TEXT}")
        );
        assert_eq!(chips.spans[1].style, Tone::Quiet.style());

        let [(lines, searched)] = details(&decisions).try_into().unwrap();
        let title = format!(
            "{}{DECISION_SEPARATOR}{NO_NOTES}",
            DecisionFeature::MemoryRecall.label()
        );
        assert_eq!(searched, format!("{title}\n{NEAR_MISS}"));
        assert_eq!(
            text(&lines[0].spans),
            format!("{DECISION_PREFIX}{title}{DECISION_SEPARATOR}{WAITED_TEXT}")
        );
        assert!(text(&lines[1].spans).ends_with(NEAR_MISS));
    }

    #[test]
    fn a_skip_is_untimed_and_never_doubles_what_the_row_already_says() {
        let feature = DecisionFeature::MemoryRecall;
        let mut decisions = Vec::new();

        for reason in [NEAR_MISS, DETAIL] {
            skipped(
                &mut decisions,
                DecisionSubject::Turn,
                feature.clone(),
                None,
                &[reason.to_owned()],
            );
        }

        let [skip] = decisions.as_slice() else {
            panic!("one chip per feature, got {}", decisions.len());
        };
        assert!(matches!(
            skip.state,
            DecisionState::Quiet { took: None, .. }
        ));
        assert_eq!(
            text(&chip_spans(&decisions).spans),
            format!(" {DECISION_PREFIX}{ALL_QUIET}")
        );

        let mut recalled = vec![settled(
            DecisionSubject::Turn,
            feature.clone(),
            NoteOutcome::Acted,
        )];
        skipped(
            &mut recalled,
            DecisionSubject::Turn,
            feature,
            None,
            &[NEAR_MISS.to_owned()],
        );
        assert_eq!(recalled.len(), 1);
    }

    #[test_case(DecisionSubject::Turn, DecisionFeature::ContentScreening => true ; "a_turns_wait")]
    #[test_case(call(CALL), DecisionFeature::MemoryRecall => true ; "a_recall_waiting_on_a_card")]
    #[test_case(call(CALL), DecisionFeature::ContentScreening => false ; "a_calls_own_wait")]
    fn only_a_calls_own_wait_ends_without_a_trace(
        subject: DecisionSubject,
        feature: DecisionFeature,
    ) -> bool {
        keeps_quiet(&subject, &feature)
    }

    #[test]
    fn a_quiet_answer_on_a_card_names_what_started_it() {
        let feature = DecisionFeature::MemoryRecall;
        let mut decisions = vec![pending(call(CALL), feature.clone())];

        assert!(!quiet(
            &mut decisions,
            &DecisionSubject::Turn,
            &feature,
            None,
            &[]
        ));
        assert!(quiet(
            &mut decisions,
            &call(CALL),
            &feature,
            Some(CAUSE),
            &[NEAR_MISS.into()]
        ));

        let [(_, searched)] = details(&decisions).try_into().unwrap();
        assert_eq!(
            searched,
            format!(
                "{}{DECISION_SEPARATOR}{CAUSE}{DECISION_SEPARATOR}{NO_NOTES}\n{NEAR_MISS}",
                feature.label()
            )
        );
    }

    /// Every chip on a row quiet reads as one dim chip timed by the longest;
    /// one that did something leaves the quiet ones out of the header.
    #[test]
    fn quiet_chips_collapse_and_stay_out_of_a_header_with_news() {
        let _clock = FrozenClock::at(WAITED);
        let recall = quieted(DecisionFeature::MemoryRecall);
        let all_quiet = [recall.clone(), quieted(DecisionFeature::SkillSuggestions)];
        assert_eq!(
            text(&chip_spans(&all_quiet).spans),
            format!(" {DECISION_PREFIX}{ALL_QUIET} {WAITED_TEXT}")
        );
        assert!(
            details(&all_quiet)
                .iter()
                .any(|(_, searched)| searched.contains(NO_SKILL))
        );

        let mixed = [
            recall,
            settled(
                DecisionSubject::Turn,
                DecisionFeature::SkillSuggestions,
                NoteOutcome::Acted,
            ),
        ];
        assert_eq!(
            text(&chip_spans(&mixed).spans),
            format!(" {DECISION_PREFIX}decided")
        );
    }

    #[test]
    fn a_rows_chips_come_whole_then_counted_with_nothing_in_front() {
        let decisions = [settled(
            DecisionSubject::Turn,
            DecisionFeature::MemoryRecall,
            NoteOutcome::Acted,
        )];

        let [whole, counted] = row_chips(&decisions);

        assert_eq!(text(&whole.spans), format!("{DECISION_PREFIX}recalled"));
        assert_eq!(
            text(&counted.spans),
            format!("{DECISION_PREFIX}1 {ONE_DECISION}")
        );
        assert_eq!(whole.width(), text(&whole.spans).chars().count());
    }
}
