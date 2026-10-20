//! What the decision engine decided, or is deciding, about a call, drawn on
//! the call's own card: a chip on its header and a section at the top of its
//! opened body. Decisions about the turn rather than a call keep their row.

use std::collections::HashMap;
use std::sync::Arc;

use caudra_agent::decisions::{DecisionFeature, NoteOutcome};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::tool_display::{DECISION_BODY_INDENT, DECISION_PREFIX, decision_style};
use super::{DECISION_SEPARATOR, SHADOW_MARKER};
use crate::theme;

const CHIP_GAP: &str = " ";
const CHIP_SEPARATOR: &str = " · ";
const PENDING_SUFFIX: &str = "…";
/// Past this many chips a header says how many there are instead.
const MAX_CHIPS: usize = 2;
const MANY_DECISIONS: &str = "decisions";
const CHILD_SEPARATOR: char = ':';

/// One decision on a card. `call_id` is the call it is about, which for a
/// batch child is the child's id rather than the card's.
#[derive(Debug, Clone, PartialEq)]
pub struct CardDecision {
    pub call_id: String,
    pub feature: DecisionFeature,
    pub state: DecisionState,
}

/// The decisions about a batch's children, by index in its roster.
pub type ChildDecisions = Arc<HashMap<usize, Vec<CardDecision>>>;

#[derive(Debug, Clone, PartialEq)]
pub enum DecisionState {
    /// The engine has not answered yet.
    Pending,
    /// `text` is what a standalone row would hold: its title, then a detail
    /// per line.
    Settled { outcome: NoteOutcome, text: String },
}

impl CardDecision {
    pub fn settled(
        call_id: String,
        feature: DecisionFeature,
        outcome: NoteOutcome,
        summary: &str,
        details: &[String],
    ) -> Self {
        let text = decision_text(feature.label(), outcome, summary, details);
        Self {
            call_id,
            feature,
            state: DecisionState::Settled { outcome, text },
        }
    }

    fn is_pending_for(&self, call_id: &str, feature: &DecisionFeature) -> bool {
        self.state == DecisionState::Pending && self.call_id == call_id && self.feature == *feature
    }

    fn chip(&self) -> (String, Tone) {
        let words = chip_words(&self.feature);
        match &self.state {
            DecisionState::Pending => (format!("{}{PENDING_SUFFIX}", words.pending), Tone::Pending),
            DecisionState::Settled { outcome, .. } => match outcome {
                NoteOutcome::Acted if words.loud => (words.acted.to_owned(), Tone::Loud),
                NoteOutcome::Acted => (words.acted.to_owned(), Tone::Acted),
                NoteOutcome::Shadow => (words.shadow.to_owned(), Tone::Shadow),
                NoteOutcome::FellBack => (words.failed.to_owned(), Tone::Loud),
            },
        }
    }
}

/// Puts `decision` in the pending slot it answers, or after the rest.
pub fn record(decisions: &mut Vec<CardDecision>, decision: CardDecision) {
    match decisions
        .iter_mut()
        .find(|held| held.is_pending_for(&decision.call_id, &decision.feature))
    {
        Some(slot) => *slot = decision,
        None => decisions.push(decision),
    }
}

/// Drops the wait for `feature` on `call_id`, saying whether there was one.
pub fn settle(decisions: &mut Vec<CardDecision>, call_id: &str, feature: &DecisionFeature) -> bool {
    let before = decisions.len();
    decisions.retain(|held| !held.is_pending_for(call_id, feature));
    decisions.len() != before
}

/// Drops every wait left once the card's call is over.
pub fn settle_all(decisions: &mut Vec<CardDecision>) -> bool {
    let before = decisions.len();
    decisions.retain(|held| held.state != DecisionState::Pending);
    decisions.len() != before
}

/// The decisions about `call_id` itself, not its children.
pub fn about<'a>(
    decisions: &'a [CardDecision],
    call_id: &'a str,
) -> impl Iterator<Item = &'a CardDecision> {
    decisions
        .iter()
        .filter(move |decision| decision.call_id == call_id)
}

/// A chip for each decision, led by a gap, or nothing.
pub fn chip_spans<'a>(decisions: impl IntoIterator<Item = &'a CardDecision>) -> Vec<Span<'static>> {
    let chips: Vec<(String, Tone)> = decisions.into_iter().map(CardDecision::chip).collect();
    if chips.len() > MAX_CHIPS {
        let tone = chips
            .iter()
            .map(|(_, tone)| *tone)
            .max()
            .unwrap_or(Tone::Acted);
        return vec![
            Span::raw(CHIP_GAP),
            Span::styled(
                format!("{DECISION_PREFIX}{} {MANY_DECISIONS}", chips.len()),
                tone.style(),
            ),
        ];
    }
    let mut spans = Vec::with_capacity(chips.len() * 2);
    for (index, (word, tone)) in chips.into_iter().enumerate() {
        spans.push(match index {
            0 => Span::raw(CHIP_GAP),
            _ => Span::styled(CHIP_SEPARATOR, theme::current().tool_dim),
        });
        spans.push(Span::styled(
            format!("{DECISION_PREFIX}{word}"),
            tone.style(),
        ));
    }
    spans
}

/// The decisions about the children of the batch `root`, by index in its
/// roster.
pub fn children(decisions: &[CardDecision], root: &str) -> ChildDecisions {
    let mut children: HashMap<usize, Vec<CardDecision>> = HashMap::new();
    for decision in decisions {
        if let Some(index) = decision
            .call_id
            .strip_prefix(root)
            .and_then(|rest| rest.strip_prefix(CHILD_SEPARATOR))
            .and_then(|index| index.parse().ok())
        {
            children.entry(index).or_default().push(decision.clone());
        }
    }
    Arc::new(children)
}

/// Whether opening the card would show a decision.
pub fn any_settled(decisions: &[CardDecision]) -> bool {
    decisions
        .iter()
        .any(|decision| decision.state != DecisionState::Pending)
}

/// Every settled decision on the card as an open row draws it, with the text
/// each one copies and searches as.
pub fn section(decisions: &[CardDecision]) -> Vec<(Vec<Line<'static>>, &str)> {
    decisions
        .iter()
        .filter_map(|decision| match &decision.state {
            DecisionState::Settled { outcome, text, .. } => {
                Some((decision_lines(text, *outcome, true), text.as_str()))
            }
            DecisionState::Pending => None,
        })
        .collect()
}

/// A decision's title behind its prefix, and once `open`, each detail under
/// it.
pub fn decision_lines(text: &str, outcome: NoteOutcome, open: bool) -> Vec<Line<'static>> {
    let style = decision_style(outcome);
    let mut lines = text.lines();
    let mut drawn = vec![Line::from(vec![
        Span::styled(style.prefix, style.prefix_style),
        Span::styled(
            lines.next().unwrap_or_default().to_owned(),
            style.text_style,
        ),
    ])];
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
    Shadow,
    Pending,
    Acted,
    Loud,
}

impl Tone {
    fn style(self) -> Style {
        let theme = theme::current();
        match self {
            Self::Shadow => theme.status_dim.add_modifier(Modifier::DIM),
            Self::Pending => theme.spinner,
            Self::Acted => theme.accent,
            Self::Loud => theme.tool_warning,
        }
    }
}

/// What a feature's chip says while it waits, once it acted, in shadow, and
/// when the engine failed. `loud` features act by warning the reader, so an
/// acted chip warns too.
struct ChipWords {
    pending: &'static str,
    acted: &'static str,
    shadow: &'static str,
    failed: &'static str,
    loud: bool,
}

const fn words(
    pending: &'static str,
    acted: &'static str,
    shadow: &'static str,
    failed: &'static str,
) -> ChipWords {
    ChipWords {
        pending,
        acted,
        shadow,
        failed,
        loud: false,
    }
}

const fn loud(words: ChipWords) -> ChipWords {
    ChipWords {
        loud: true,
        ..words
    }
}

const RANKING: ChipWords = words("ranking", "ranked", "would rank", "ranking failed");
const DECIDING: ChipWords = words("deciding", "decided", "would decide", "decision failed");

fn chip_words(feature: &DecisionFeature) -> ChipWords {
    match feature {
        DecisionFeature::SourceCodeContext => {
            words("reranking", "reranked", "would rerank", "rerank failed")
        }
        DecisionFeature::MemorySearch | DecisionFeature::HistorySearch => RANKING,
        DecisionFeature::MemoryDuplicates => words(
            "checking duplicates",
            "may duplicate",
            "would flag duplicates",
            "duplicate check failed",
        ),
        DecisionFeature::AutoScreening => loud(words(
            "screening",
            "asked you",
            "would ask",
            "screening failed",
        )),
        DecisionFeature::ShellEffect => loud(words(
            "checking writes",
            "may write",
            "would warn",
            "write check failed",
        )),
        DecisionFeature::ContentScreening => loud(words(
            "screening result",
            "flagged",
            "would flag",
            "screening failed",
        )),
        DecisionFeature::OutputPointers => words(
            "finding omitted lines",
            "pointed",
            "would point",
            "pointing failed",
        ),
        DecisionFeature::SubagentRouting => {
            words("routing", "routed", "would route", "routing failed")
        }
        DecisionFeature::Workflow
        | DecisionFeature::Code
        | DecisionFeature::PermissionAdvice
        | DecisionFeature::SkillSuggestions
        | DecisionFeature::GoalPrescreen
        | DecisionFeature::QuestionToolNudge
        | DecisionFeature::MemoryRecall
        | DecisionFeature::CompactionRetention => DECIDING,
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::decisions::{DecisionFeature, NoteOutcome};
    use ratatui::text::Span;
    use test_case::test_case;

    use super::{
        CardDecision, DecisionState, MANY_DECISIONS, Tone, about, any_settled, children,
        chip_spans, record, section, settle, settle_all,
    };
    use crate::components::tool_display::DECISION_PREFIX;

    const CALL: &str = "t1";
    const CHILD: &str = "t1:2";
    const GRANDCHILD: &str = "t1:2:0";
    const SUMMARY: &str = "moved 5 of 12 rows up";
    const DETAIL: &str = "0.97  src/lib.rs:12 chip  moved 4th to 1st";

    fn settled(call_id: &str, feature: DecisionFeature, outcome: NoteOutcome) -> CardDecision {
        CardDecision::settled(call_id.into(), feature, outcome, SUMMARY, &[DETAIL.into()])
    }

    fn pending(call_id: &str, feature: DecisionFeature) -> CardDecision {
        CardDecision {
            call_id: call_id.into(),
            feature,
            state: DecisionState::Pending,
        }
    }

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test_case(pending(CALL, DecisionFeature::SourceCodeContext), "reranking…", Tone::Pending ; "a_wait")]
    #[test_case(settled(CALL, DecisionFeature::SourceCodeContext, NoteOutcome::Acted), "reranked", Tone::Acted ; "acted")]
    #[test_case(settled(CALL, DecisionFeature::MemorySearch, NoteOutcome::Shadow), "would rank", Tone::Shadow ; "shadow")]
    #[test_case(settled(CALL, DecisionFeature::OutputPointers, NoteOutcome::FellBack), "pointing failed", Tone::Loud ; "a_fallback_warns")]
    #[test_case(settled(CALL, DecisionFeature::AutoScreening, NoteOutcome::Acted), "asked you", Tone::Loud ; "a_screening_that_acted_warns")]
    #[test_case(settled(CALL, DecisionFeature::ContentScreening, NoteOutcome::Shadow), "would flag", Tone::Shadow ; "a_shadow_screening_stays_quiet")]
    fn a_chip_says_what_the_feature_did(decision: CardDecision, word: &str, tone: Tone) {
        assert_eq!(decision.chip(), (word.to_owned(), tone));
        assert_eq!(
            text(&chip_spans([&decision])),
            format!(" {DECISION_PREFIX}{word}")
        );
    }

    #[test]
    fn two_chips_join_and_more_are_counted_in_the_loudest_tone() {
        let mut decisions = vec![
            settled(CALL, DecisionFeature::OutputPointers, NoteOutcome::Acted),
            settled(CALL, DecisionFeature::AutoScreening, NoteOutcome::Acted),
        ];
        assert_eq!(
            text(&chip_spans(&decisions)),
            format!(" {DECISION_PREFIX}pointed · {DECISION_PREFIX}asked you")
        );

        decisions.push(pending(CALL, DecisionFeature::ShellEffect));
        let spans = chip_spans(&decisions);
        assert_eq!(
            text(&spans),
            format!(" {DECISION_PREFIX}3 {MANY_DECISIONS}")
        );
        assert_eq!(spans[1].style, Tone::Loud.style());
    }

    #[test]
    fn an_answer_takes_its_waits_place_and_a_quiet_one_drops_it() {
        let feature = DecisionFeature::MemorySearch;
        let mut decisions = vec![
            pending(CALL, feature.clone()),
            pending(CALL, DecisionFeature::OutputPointers),
        ];

        record(
            &mut decisions,
            settled(CALL, feature.clone(), NoteOutcome::Acted),
        );
        assert_eq!(
            decisions,
            [
                settled(CALL, feature.clone(), NoteOutcome::Acted),
                pending(CALL, DecisionFeature::OutputPointers),
            ]
        );

        assert!(!settle(&mut decisions, CALL, &feature));
        assert!(settle(
            &mut decisions,
            CALL,
            &DecisionFeature::OutputPointers
        ));
        assert_eq!(decisions, [settled(CALL, feature, NoteOutcome::Acted)]);
    }

    #[test]
    fn the_call_ending_drops_every_wait_but_keeps_what_was_decided() {
        let kept = settled(CHILD, DecisionFeature::MemorySearch, NoteOutcome::Acted);
        let mut decisions = vec![pending(CALL, DecisionFeature::OutputPointers), kept.clone()];

        assert!(settle_all(&mut decisions));
        assert_eq!(decisions, [kept]);
    }

    #[test]
    fn a_batch_hands_each_direct_child_its_own_decisions() {
        let mine = settled(CALL, DecisionFeature::SubagentRouting, NoteOutcome::Acted);
        let child = settled(CHILD, DecisionFeature::MemorySearch, NoteOutcome::Acted);
        let grandchild = settled(
            GRANDCHILD,
            DecisionFeature::MemorySearch,
            NoteOutcome::Acted,
        );
        let decisions = [mine.clone(), child.clone(), grandchild];

        assert_eq!(about(&decisions, CALL).collect::<Vec<_>>(), [&mine]);
        let children = children(&decisions, CALL);
        assert_eq!(children.len(), 1);
        assert_eq!(children[&2], [child]);
    }

    #[test]
    fn only_a_settled_decision_has_a_section() {
        let waiting = [pending(CALL, DecisionFeature::SourceCodeContext)];
        assert!(!any_settled(&waiting));
        assert!(section(&waiting).is_empty());

        let decided = [settled(
            CALL,
            DecisionFeature::SourceCodeContext,
            NoteOutcome::Acted,
        )];
        let [(lines, text)] = section(&decided).try_into().unwrap();
        assert!(any_settled(&decided));
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with(&format!("{SUMMARY}\n{DETAIL}")));
    }
}
