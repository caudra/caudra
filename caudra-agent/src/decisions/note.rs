use std::mem;

use caudra_config::decisions::FeatureMode;
use caudra_storage::engine_decisions::{
    StoredDecisionAnchor, StoredDecisionOutcome, StoredEngineDecision,
};
use serde::{Serialize, Serializer};

use super::{DecisionFeature, Decisions, candidate_text};
use crate::{AgentEvent, EventSender};

const MAX_SUMMARY_BYTES: usize = 200;
const MAX_DETAIL_BYTES: usize = 200;
const MAX_DETAIL_LINES: usize = 20;
const MAX_NOTE_BYTES: usize = 4_096;
const ELLIPSIS: &str = "…";
const CONTROL_REPLACEMENT: char = ' ';
const MORE_PREFIX: &str = "+";
const MORE_SUFFIX: &str = " more";
const MAX_COUNT_DIGITS: usize = usize::MAX.ilog10() as usize + 1;
const MORE_LINE_BYTES: usize = MORE_PREFIX.len() + MAX_COUNT_DIGITS + MORE_SUFFIX.len();
/// The store numbers rows as it inserts them.
const UNASSIGNED_SEQ: u64 = 0;
const MAX_LABEL_BYTES: usize = 120;
const SHADOW_PREFIX: &str = "would have ";
const NO_LEAD: &str = "";
const FALLBACK_SEPARATOR: &str = "; ";
const COLUMN_SEPARATOR: &str = "  ";
const ORDINAL_SUFFIXES: [&str; 4] = ["th", "st", "nd", "rd"];
const KEPT_VERB: &str = "kept";
/// Between what started a decision and what it did: `step started · named 2 notes`.
const CAUSE_SEPARATOR: &str = " · ";
const MOVED_VERB: &str = "moved";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteOutcome {
    /// The engine's answer changed what happened.
    Acted,
    /// Shadow mode: what the answer would have changed.
    Shadow,
    /// The engine failed, so the default applied.
    FellBack,
}

impl NoteOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acted => "acted",
            Self::Shadow => "shadow",
            Self::FellBack => "fell_back",
        }
    }

    /// What a summary opens with before what the surface did: a shadow row
    /// tells what it would have done.
    pub(crate) const fn lead(self) -> &'static str {
        match self {
            Self::Shadow => SHADOW_PREFIX,
            Self::Acted | Self::FellBack => NO_LEAD,
        }
    }
}

/// Where a note sits among the turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteAnchor {
    /// Before the transcript item at this index, counted across compaction
    /// seams.
    History(usize),
    /// After the card of the call with this tool use id.
    Tool(String),
}

/// What a decision feature did, for a display-only transcript row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionNote {
    /// Serialized as the name the decision log records.
    #[serde(serialize_with = "feature_name")]
    pub feature: DecisionFeature,
    pub mode: FeatureMode,
    pub outcome: NoteOutcome,
    /// What happened. The row's title names the feature, so this does not.
    pub summary: String,
    /// One candidate or value per line.
    pub details: Vec<String>,
    pub anchor: NoteAnchor,
}

impl DecisionNote {
    /// Every line is single, free of control characters and cut on a
    /// character boundary. At most [`MAX_DETAIL_LINES`] details are kept, the
    /// last counting any left out, and the text stays within
    /// [`MAX_NOTE_BYTES`].
    pub fn new(
        feature: DecisionFeature,
        mode: FeatureMode,
        outcome: NoteOutcome,
        summary: &str,
        details: Vec<String>,
        anchor: NoteAnchor,
    ) -> Self {
        let summary = single_line(summary, MAX_SUMMARY_BYTES);
        let details = bounded_details(details, MAX_NOTE_BYTES - summary.len());
        Self {
            feature,
            mode,
            outcome,
            summary,
            details,
            anchor,
        }
    }

    /// The row a session keeps for this note, made at `at_ms` for `agent`,
    /// `None` being the main agent.
    pub fn stored(&self, at_ms: u64, agent: Option<String>) -> StoredEngineDecision {
        StoredEngineDecision {
            seq: UNASSIGNED_SEQ,
            at_ms,
            feature: self.feature.name().to_owned(),
            mode: self.mode.as_str().to_owned(),
            outcome: self.outcome.into(),
            summary: self.summary.clone(),
            details: self.details.clone(),
            agent,
            anchor: match &self.anchor {
                NoteAnchor::History(len) => StoredDecisionAnchor::History(*len),
                NoteAnchor::Tool(id) => StoredDecisionAnchor::Tool(id.clone()),
            },
        }
    }
}

impl From<NoteOutcome> for StoredDecisionOutcome {
    fn from(outcome: NoteOutcome) -> Self {
        match outcome {
            NoteOutcome::Acted => Self::Acted,
            NoteOutcome::Shadow => Self::Shadow,
            NoteOutcome::FellBack => Self::FellBack,
        }
    }
}

impl From<StoredDecisionOutcome> for NoteOutcome {
    fn from(outcome: StoredDecisionOutcome) -> Self {
        match outcome {
            StoredDecisionOutcome::Acted => Self::Acted,
            StoredDecisionOutcome::Shadow => Self::Shadow,
            StoredDecisionOutcome::FellBack => Self::FellBack,
        }
    }
}

/// A row before it is placed: the surface that knows what happened is not
/// always the one that knows where the row sits.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteDraft {
    pub feature: DecisionFeature,
    pub mode: FeatureMode,
    pub outcome: NoteOutcome,
    pub summary: String,
    pub details: Vec<String>,
}

impl NoteDraft {
    /// `acted` says what the surface did, in the past tense; a shadow row
    /// reads it as what the surface would have done.
    pub fn new(
        decisions: &Decisions,
        feature: DecisionFeature,
        outcome: NoteOutcome,
        acted: &str,
        details: Vec<String>,
    ) -> Self {
        Self {
            mode: decisions.mode(&feature).clone(),
            feature,
            outcome,
            summary: format!("{}{acted}", outcome.lead()),
            details,
        }
    }

    /// The engine's `failure`, then the `default` the surface kept instead.
    pub fn fell_back(
        decisions: &Decisions,
        feature: DecisionFeature,
        failure: &str,
        default: &str,
    ) -> Self {
        let summary = format!("{failure}{FALLBACK_SEPARATOR}{default}");
        Self::new(
            decisions,
            feature,
            NoteOutcome::FellBack,
            &summary,
            Vec::new(),
        )
    }

    pub fn anchored(self, anchor: NoteAnchor) -> DecisionNote {
        DecisionNote::new(
            self.feature,
            self.mode,
            self.outcome,
            &self.summary,
            self.details,
            anchor,
        )
    }
}

/// Places a surface's drafts at one anchor.
#[derive(Clone)]
pub struct NoteSink {
    event_tx: EventSender,
    anchor: NoteAnchor,
    cause: Option<&'static str>,
}

impl NoteSink {
    pub fn new(event_tx: EventSender, anchor: NoteAnchor) -> Self {
        Self {
            event_tx,
            anchor,
            cause: None,
        }
    }

    /// Names what started the decisions placed here, which leads each row's
    /// summary and rides on each wait, for a surface that runs for more than
    /// one reason.
    pub fn caused(self, cause: Option<&'static str>) -> Self {
        Self { cause, ..self }
    }

    pub fn emit(&self, mut draft: NoteDraft) {
        if let Some(cause) = self.cause {
            draft.summary = format!("{cause}{CAUSE_SEPARATOR}{}", draft.summary);
        }
        self.event_tx.try_send(draft.anchored(self.anchor.clone()));
    }

    /// Says `feature` is asking the engine about this anchor, when it will:
    /// a feature that is off or has no engine never waits, so nothing is said.
    pub fn begin(&self, decisions: &Decisions, feature: DecisionFeature) -> PendingNote {
        let pending = decisions.enabled(&feature);
        if pending {
            self.event_tx.try_send(AgentEvent::DecisionPending(Box::new(
                self.pending(&feature, Vec::new()),
            )));
        }
        PendingNote {
            sink: Some(self.clone()),
            feature,
            pending,
            near_misses: Vec::new(),
        }
    }

    /// Says `feature` looked at this anchor and had nothing to ask the engine,
    /// `reason` saying why: a settle with no wait before it, so no time.
    pub fn skipped(&self, decisions: &Decisions, feature: DecisionFeature, reason: &str) {
        if decisions.enabled(&feature) {
            self.event_tx.try_send(AgentEvent::DecisionSettled(Box::new(
                self.pending(&feature, vec![reason.to_owned()]),
            )));
        }
    }

    fn pending(&self, feature: &DecisionFeature, details: Vec<String>) -> PendingDecision {
        PendingDecision {
            feature: feature.clone(),
            anchor: self.anchor.clone(),
            cause: self.cause,
            details,
        }
    }
}

/// A decision a call waits on, so its card can say so until the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingDecision {
    #[serde(serialize_with = "feature_name")]
    pub feature: DecisionFeature,
    pub anchor: NoteAnchor,
    /// What started it, when its feature runs for more than one reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<&'static str>,
    /// Empty while it waits. Settled without a note, what the engine weighed
    /// and found short of the bar, one candidate per line, or, settled without
    /// a wait, why the engine was not asked. Never stored.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

/// Holds a [`PendingDecision`] open. Emitting the note settles it; dropped
/// without one, because the engine stayed quiet, nothing changed, or the call
/// was cancelled, it says the wait is over.
pub struct PendingNote {
    sink: Option<NoteSink>,
    feature: DecisionFeature,
    pending: bool,
    near_misses: Vec<String>,
}

impl PendingNote {
    /// For a surface with nowhere to place a row.
    pub fn unanchored(feature: DecisionFeature) -> Self {
        Self {
            sink: None,
            feature,
            pending: false,
            near_misses: Vec::new(),
        }
    }

    /// Ends the wait without a note, saying what came closest.
    pub fn quiet(mut self, near_misses: Vec<String>) {
        self.near_misses = bounded_details(near_misses, MAX_NOTE_BYTES);
    }

    pub fn emit(mut self, draft: NoteDraft) {
        self.pending &= draft.feature != self.feature;
        if let Some(sink) = &self.sink {
            sink.emit(draft);
        }
    }

    pub fn settle(self, draft: Option<NoteDraft>) {
        if let Some(draft) = draft {
            self.emit(draft);
        }
    }
}

impl Drop for PendingNote {
    fn drop(&mut self) {
        if let Some(sink) = self.sink.as_ref().filter(|_| self.pending) {
            sink.event_tx.try_send(AgentEvent::DecisionSettled(Box::new(
                sink.pending(&self.feature, mem::take(&mut self.near_misses)),
            )));
        }
    }
}

/// `0.97  42+1 release-preview-7  kept 1st`: a candidate's score, the label
/// the transcript already shows it by, redacted and bounded, and what became
/// of it.
pub fn score_line(score: f64, label: &str, action: &str) -> String {
    let label = candidate_text(label, MAX_LABEL_BYTES);
    format!("{score:.2}{COLUMN_SEPARATOR}{label}{COLUMN_SEPARATOR}{action}")
}

/// `1st`, `2nd`, `11th`, for the position at 0-based `index`.
fn ordinal(index: usize) -> String {
    let position = index + 1;
    let suffix = match (position % 10, position % 100) {
        (_, 11..=13) => ORDINAL_SUFFIXES[0],
        (last @ 1..=3, _) => ORDINAL_SUFFIXES[last],
        _ => ORDINAL_SUFFIXES[0],
    };
    format!("{position}{suffix}")
}

/// `kept 1st` or `moved 3rd to 1st`, for a candidate that went from 0-based
/// `from` to `to`.
pub fn placement(from: usize, to: usize) -> String {
    match from == to {
        true => format!("{KEPT_VERB} {}", ordinal(to)),
        false => format!("{MOVED_VERB} {} to {}", ordinal(from), ordinal(to)),
    }
}

/// `1 note` or `3 notes`.
pub fn counted(count: usize, one: &str, many: &str) -> String {
    match count {
        1 => format!("{count} {one}"),
        _ => format!("{count} {many}"),
    }
}

fn feature_name<S: Serializer>(
    feature: &DecisionFeature,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(feature.name())
}

/// Each line that fits in `budget` bytes while leaving room to count the
/// rest, and the count once one does not.
fn bounded_details(details: Vec<String>, mut budget: usize) -> Vec<String> {
    let total = details.len();
    let mut kept = Vec::with_capacity(total.min(MAX_DETAIL_LINES));
    for (index, detail) in details.into_iter().enumerate() {
        let remaining = total - index;
        let last = remaining == 1;
        let reserve = if last { 0 } else { MORE_LINE_BYTES };
        let line = (last || kept.len() + 1 < MAX_DETAIL_LINES)
            .then(|| single_line(&detail, MAX_DETAIL_BYTES))
            .filter(|line| line.len() + reserve <= budget);
        let Some(line) = line else {
            kept.push(format!("{MORE_PREFIX}{remaining}{MORE_SUFFIX}"));
            break;
        };
        budget -= line.len();
        kept.push(line);
    }
    kept
}

/// `text` on one line with its control characters blanked, cut to `limit`
/// bytes on a character boundary and marked when cut.
fn single_line(text: &str, limit: usize) -> String {
    let text = text.trim();
    let end = match text.len() > limit {
        true => text.floor_char_boundary(limit - ELLIPSIS.len()),
        false => text.len(),
    };
    let mut line: String = text[..end]
        .chars()
        .map(|character| match character.is_control() {
            true => CONTROL_REPLACEMENT,
            false => character,
        })
        .collect();
    if end < text.len() {
        line.push_str(ELLIPSIS);
    }
    line
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::{
        DecisionEndpoint, DecisionProtocol, DecisionsConfig, FeatureMode,
    };
    use caudra_storage::engine_decisions::{
        StoredDecisionAnchor, StoredDecisionOutcome, StoredEngineDecision,
    };
    use serde_json::json;
    use test_case::test_case;

    use caudra_storage::StateDir;
    use flume::Receiver;
    use tempfile::TempDir;

    use super::{
        CAUSE_SEPARATOR, DecisionNote, ELLIPSIS, MAX_DETAIL_BYTES, MAX_DETAIL_LINES,
        MAX_NOTE_BYTES, MAX_SUMMARY_BYTES, NoteAnchor, NoteDraft, NoteOutcome, NoteSink,
        UNASSIGNED_SEQ, placement,
    };
    use crate::decisions::{DecisionFeature, Decisions};
    use crate::{AgentEvent, Envelope, EventSender};

    const SUMMARY: &str = "moved 3 of 12 rows up";
    const DETAIL: &str = "0.97  42+1 release-preview-7  kept 1st";
    const CONTROLLED: &str = "\n kept\u{1b}[31m 1st\r\n\tof\u{7}12 \n";
    const CONTROL_FREE: &str = "kept [31m 1st   of 12";
    const WIDE: &str = "é";
    const NARROW: &str = "x";
    const ONE_MORE: &str = "+1 more";
    const HISTORY_LEN: usize = 38;
    const TOOL_USE_ID: &str = "toolu_01";
    const AT_MS: u64 = 1_767_225_600_000;
    const AGENT: &str = "task-1";
    const BASE_URL: &str = "http://127.0.0.1:1";
    const PENDING: &str = "pending";
    const SETTLED: &str = "settled";
    const CAUSE: &str = "step started";

    fn note(summary: &str, details: Vec<String>) -> DecisionNote {
        DecisionNote::new(
            DecisionFeature::SourceCodeContext,
            FeatureMode::Enforce,
            NoteOutcome::Acted,
            summary,
            details,
            NoteAnchor::Tool(TOOL_USE_ID.into()),
        )
    }

    fn details(count: usize, text: &str) -> Vec<String> {
        vec![text.to_owned(); count]
    }

    fn text_bytes(note: &DecisionNote) -> usize {
        note.summary.len() + note.details.iter().map(String::len).sum::<usize>()
    }

    #[test]
    fn a_long_line_is_cut_on_a_char_boundary() {
        let long = WIDE.repeat(MAX_SUMMARY_BYTES.max(MAX_DETAIL_BYTES));
        let note = note(&long, details(1, &long));
        for (line, limit) in [
            (&note.summary, MAX_SUMMARY_BYTES),
            (&note.details[0], MAX_DETAIL_BYTES),
        ] {
            assert!(line.len() <= limit);
            let kept = line.strip_suffix(ELLIPSIS).unwrap();
            assert!(!kept.is_empty());
            assert!(long.starts_with(kept));
        }
    }

    #[test]
    fn control_characters_never_reach_a_line() {
        let note = note(CONTROLLED, details(1, CONTROLLED));
        assert_eq!(note.summary, CONTROL_FREE);
        assert_eq!(note.details, [CONTROL_FREE]);
    }

    #[test_case(MAX_DETAIL_LINES, MAX_DETAIL_LINES, None; "a_full_list_is_kept")]
    #[test_case(MAX_DETAIL_LINES + 1, MAX_DETAIL_LINES, Some("+2 more"); "one_over_counts_two")]
    #[test_case(MAX_DETAIL_LINES * 3, MAX_DETAIL_LINES, Some("+41 more"); "many_over")]
    fn details_are_capped_with_a_count_of_the_rest(count: usize, lines: usize, more: Option<&str>) {
        let note = note(SUMMARY, details(count, DETAIL));
        assert_eq!(note.details.len(), lines);
        assert_eq!(
            note.details.last().unwrap().as_str(),
            more.unwrap_or(DETAIL)
        );
    }

    #[test]
    fn the_whole_note_stays_within_its_byte_cap() {
        let long = NARROW.repeat(MAX_DETAIL_BYTES * 2);
        let note = note(&long, details(MAX_DETAIL_LINES, &long));
        assert!(text_bytes(&note) <= MAX_NOTE_BYTES);
        assert_eq!(note.details.len(), MAX_DETAIL_LINES);
        assert_eq!(note.details.last().unwrap().as_str(), ONE_MORE);
    }

    #[test_case(0, 0, "kept 1st"; "kept")]
    #[test_case(2, 0, "moved 3rd to 1st"; "moved_up")]
    #[test_case(10, 21, "moved 11th to 22nd"; "teens_take_th")]
    fn placement_names_both_positions(from: usize, to: usize, expected: &str) {
        assert_eq!(placement(from, to), expected);
    }

    #[test_case(NoteAnchor::History(HISTORY_LEN), StoredDecisionAnchor::History(HISTORY_LEN), None ; "history_main")]
    #[test_case(NoteAnchor::Tool(TOOL_USE_ID.into()), StoredDecisionAnchor::Tool(TOOL_USE_ID.into()), Some(AGENT) ; "tool_subagent")]
    fn the_stored_row_keeps_the_logged_names_and_the_anchor(
        anchor: NoteAnchor,
        expected_anchor: StoredDecisionAnchor,
        agent: Option<&str>,
    ) {
        let note = DecisionNote::new(
            DecisionFeature::SourceCodeContext,
            FeatureMode::Shadow,
            NoteOutcome::Shadow,
            SUMMARY,
            details(1, DETAIL),
            anchor,
        );

        let stored = note.stored(AT_MS, agent.map(str::to_owned));

        assert_eq!(
            stored,
            StoredEngineDecision {
                seq: UNASSIGNED_SEQ,
                at_ms: AT_MS,
                feature: DecisionFeature::SourceCodeContext.name().into(),
                mode: FeatureMode::Shadow.as_str().into(),
                outcome: StoredDecisionOutcome::Shadow,
                summary: SUMMARY.into(),
                details: vec![DETAIL.into()],
                agent: agent.map(str::to_owned),
                anchor: expected_anchor,
            }
        );
        assert_eq!(NoteOutcome::from(stored.outcome), note.outcome);
    }

    fn sink_for(mode: FeatureMode) -> (NoteSink, Decisions, Receiver<Envelope>, TempDir) {
        let root = tempfile::tempdir().unwrap();
        let mut config = DecisionsConfig {
            endpoint: Some(DecisionEndpoint::new(
                DecisionProtocol::TypeSafe.as_str(),
                DecisionProtocol::TypeSafe,
                BASE_URL.parse().unwrap(),
            )),
            ..DecisionsConfig::default()
        };
        config.features.source_code_context = mode;
        let decisions = Decisions::new(config, &StateDir::from_path(root.path().into())).unwrap();
        let (tx, rx) = flume::unbounded();
        let sink = NoteSink::new(
            EventSender::new(tx, 0),
            NoteAnchor::Tool(TOOL_USE_ID.into()),
        );
        (sink, decisions, rx, root)
    }

    /// What the card hears, in order: `pending`, `settled`, or the note's summary.
    fn heard(events: &Receiver<Envelope>) -> Vec<String> {
        events
            .try_iter()
            .map(|envelope| match envelope.event {
                AgentEvent::DecisionPending(pending) => {
                    assert_eq!(pending.anchor, NoteAnchor::Tool(TOOL_USE_ID.into()));
                    PENDING.to_owned()
                }
                AgentEvent::DecisionSettled(_) => SETTLED.to_owned(),
                AgentEvent::Decision(note) => note.summary,
                other => panic!("unexpected event {other:?}"),
            })
            .collect()
    }

    fn draft(feature: DecisionFeature) -> NoteDraft {
        NoteDraft {
            feature,
            mode: FeatureMode::Enforce,
            outcome: NoteOutcome::Acted,
            summary: SUMMARY.into(),
            details: Vec::new(),
        }
    }

    #[test_case(Some(DecisionFeature::SourceCodeContext), &[PENDING, SUMMARY]; "its_note_settles_the_wait")]
    #[test_case(None, &[PENDING, SETTLED]; "dropped_without_one_it_says_the_wait_is_over")]
    #[test_case(Some(DecisionFeature::MemorySearch), &[PENDING, SUMMARY, SETTLED]; "another_features_note_leaves_it_waiting")]
    fn a_wait_ends_once(emitted: Option<DecisionFeature>, expected: &[&str]) {
        let (sink, decisions, events, _root) = sink_for(FeatureMode::Enforce);
        let pending = sink.begin(&decisions, DecisionFeature::SourceCodeContext);
        pending.settle(emitted.map(draft));
        assert_eq!(heard(&events), expected);
    }

    #[test]
    fn a_quiet_answer_says_what_came_closest_as_it_settles() {
        let (sink, decisions, events, _root) = sink_for(FeatureMode::Enforce);
        sink.begin(&decisions, DecisionFeature::SourceCodeContext)
            .quiet(details(1, DETAIL));
        let settled: Vec<Vec<String>> = events
            .try_iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::DecisionSettled(settled) => Some(settled.details),
                _ => None,
            })
            .collect();
        assert_eq!(settled, [details(1, DETAIL)]);
    }

    #[test]
    fn a_skip_settles_without_a_wait_and_says_why() {
        let (sink, decisions, events, _root) = sink_for(FeatureMode::Enforce);
        sink.skipped(&decisions, DecisionFeature::SourceCodeContext, DETAIL);
        let heard: Vec<(bool, Vec<String>)> = events
            .try_iter()
            .map(|envelope| match envelope.event {
                AgentEvent::DecisionSettled(settled) => (true, settled.details),
                _ => (false, Vec::new()),
            })
            .collect();
        assert_eq!(heard, [(true, details(1, DETAIL))]);
    }

    #[test]
    fn a_feature_that_is_off_never_waits() {
        let (sink, decisions, events, _root) = sink_for(FeatureMode::Off);
        drop(sink.begin(&decisions, DecisionFeature::SourceCodeContext));
        sink.skipped(&decisions, DecisionFeature::SourceCodeContext, DETAIL);
        assert!(heard(&events).is_empty());
    }

    #[test_case(NoteAnchor::History(HISTORY_LEN), json!({"history": HISTORY_LEN}); "history")]
    #[test_case(NoteAnchor::Tool(TOOL_USE_ID.into()), json!({"tool": TOOL_USE_ID}); "tool")]
    fn the_event_names_the_logged_feature_and_snake_case_outcome(
        anchor: NoteAnchor,
        expected_anchor: serde_json::Value,
    ) {
        let note = DecisionNote::new(
            DecisionFeature::GoalPrescreen,
            FeatureMode::Advise,
            NoteOutcome::FellBack,
            SUMMARY,
            details(1, DETAIL),
            anchor,
        );
        let event = serde_json::to_value(AgentEvent::from(note)).unwrap();
        assert_eq!(
            event,
            json!({
                "type": "decision",
                "feature": DecisionFeature::GoalPrescreen.name(),
                "mode": FeatureMode::Advise.as_str(),
                "outcome": NoteOutcome::FellBack.as_str(),
                "summary": SUMMARY,
                "details": [DETAIL],
                "anchor": expected_anchor,
            })
        );
    }

    #[test_case(None, None, SUMMARY.to_owned() ; "a_sink_without_a_cause_keeps_the_summary")]
    #[test_case(Some(CAUSE), Some(json!(CAUSE)), format!("{CAUSE}{CAUSE_SEPARATOR}{SUMMARY}") ; "a_caused_sink_leads_with_its_cause")]
    fn a_cause_rides_on_the_wait_and_leads_the_summary(
        cause: Option<&'static str>,
        serialized: Option<serde_json::Value>,
        summary: String,
    ) {
        let (sink, decisions, events, _root) = sink_for(FeatureMode::Enforce);
        sink.caused(cause)
            .begin(&decisions, DecisionFeature::SourceCodeContext)
            .emit(draft(DecisionFeature::SourceCodeContext));
        let heard: Vec<(Option<serde_json::Value>, Option<String>)> = events
            .try_iter()
            .map(|envelope| match envelope.event {
                AgentEvent::DecisionPending(pending) => {
                    assert_eq!(pending.cause, cause);
                    let event = serde_json::to_value(&*pending).unwrap();
                    (event.get("cause").cloned(), None)
                }
                AgentEvent::Decision(note) => (None, Some(note.summary)),
                other => panic!("unexpected event {other:?}"),
            })
            .collect();
        assert_eq!(heard, [(serialized, None), (None, Some(summary))]);
    }
}
