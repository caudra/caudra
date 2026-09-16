mod anti_unify;
mod evidence;
mod index;
mod observation;

use caudra_storage::permission_patterns::{
    MAX_PATTERN_TUPLES, PatternDefinition, PatternValidationError,
};
use serde::Serialize;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};
use thiserror::Error;

use anti_unify::anti_unify;
use evidence::evidence;
pub use evidence::{CandidateEvidence, SupportCount, TupleSupport};
use index::{ObservationKey, ShapeKey};
#[cfg(test)]
pub(super) use observation::fixtures;
pub use observation::{
    CommandObservation, InvocationOutcome, MAX_OBSERVATION_ID_BYTES, MAX_OBSERVATION_JSON_BYTES,
    MAX_TIMESTAMP_MS, OBSERVATION_SCHEMA_VERSION, ObservationError, ObservationProvenance,
    ObservationSource, ObservationVerification, ShellEffectStatus,
};

pub const MAX_RECOGNIZER_OBSERVATIONS: usize = 4096;
pub const MAX_RECOGNIZER_CLUSTERS: usize = 256;
pub const MAX_RECOGNIZER_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RECOGNIZER_SUGGESTIONS: usize = 64;
const MIN_SUPPORT: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecognizerLimits {
    pub max_observations: usize,
    pub max_clusters: usize,
    pub max_cluster_observations: usize,
    pub max_observation_bytes: usize,
    pub max_suggestions: usize,
    pub min_support: usize,
    pub min_sessions: usize,
}

impl Default for RecognizerLimits {
    fn default() -> Self {
        Self {
            max_observations: MAX_RECOGNIZER_OBSERVATIONS,
            max_clusters: MAX_RECOGNIZER_CLUSTERS,
            max_cluster_observations: MAX_PATTERN_TUPLES,
            max_observation_bytes: MAX_RECOGNIZER_BYTES,
            max_suggestions: MAX_RECOGNIZER_SUGGESTIONS,
            min_support: MIN_SUPPORT,
            min_sessions: MIN_SUPPORT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatternCandidate {
    pub definition: PatternDefinition,
    pub evidence: CandidateEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecognitionExclusion {
    UnverifiedFacts,
    SensitiveOrPayload,
    InvalidInput,
    RejectedInvocation,
    Collision,
    Capacity,
    FutureTimestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecognitionStats {
    pub received: u64,
    pub accepted: u64,
    pub duplicates: u64,
    pub exclusions: BTreeMap<RecognitionExclusion, u64>,
    pub retained_observations: usize,
    pub retained_bytes: usize,
    pub quarantined_ids: usize,
    pub clusters: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveOutcome {
    Added,
    Duplicate,
}

#[derive(Debug, Error)]
pub enum RecognitionError {
    #[error(transparent)]
    Observation(#[from] ObservationError),
    #[error(transparent)]
    Definition(#[from] PatternValidationError),
    #[error("recognizer limits must be nonzero, internally consistent, and within hard caps")]
    InvalidLimits,
    #[error("observation is newer than the fixed discovery cutoff")]
    FutureObservation,
    #[error("rejected invocations are not recurrence evidence")]
    RejectedInvocation,
    #[error("conflicting facts for one source/observation ID; both versions are quarantined")]
    Collision,
    #[error("source/observation ID was previously quarantined")]
    Quarantined,
    #[error("recognizer capacity reached: {0}")]
    Capacity(&'static str),
    #[error("recognizer structural index is inconsistent")]
    IndexInvariant,
}

struct StoredObservation {
    observation: CommandObservation,
    bytes: usize,
}

pub struct PatternRecognizer {
    limits: RecognizerLimits,
    as_of_timestamp_ms: u64,
    observations: BTreeMap<ObservationKey, StoredObservation>,
    index: BTreeMap<ShapeKey, BTreeSet<ObservationKey>>,
    quarantined: BTreeSet<ObservationKey>,
    stats: RecognitionStats,
}

impl PatternRecognizer {
    pub fn new(
        limits: RecognizerLimits,
        as_of_timestamp_ms: u64,
    ) -> Result<Self, RecognitionError> {
        for (value, cap) in [
            (limits.max_observations, MAX_RECOGNIZER_OBSERVATIONS),
            (limits.max_clusters, MAX_RECOGNIZER_CLUSTERS),
            (limits.max_cluster_observations, MAX_PATTERN_TUPLES),
            (limits.max_observation_bytes, MAX_RECOGNIZER_BYTES),
            (limits.max_suggestions, MAX_RECOGNIZER_SUGGESTIONS),
        ] {
            if value == 0 || value > cap {
                return Err(RecognitionError::InvalidLimits);
            }
        }
        if limits.min_support < MIN_SUPPORT
            || limits.min_support > limits.max_cluster_observations.min(limits.max_observations)
            || limits.min_sessions == 0
            || limits.min_sessions > limits.min_support
            || as_of_timestamp_ms == 0
            || as_of_timestamp_ms > MAX_TIMESTAMP_MS
        {
            return Err(RecognitionError::InvalidLimits);
        }
        Ok(Self {
            limits,
            as_of_timestamp_ms,
            observations: BTreeMap::new(),
            index: BTreeMap::new(),
            quarantined: BTreeSet::new(),
            stats: RecognitionStats::default(),
        })
    }

    pub fn observe(
        &mut self,
        observation: CommandObservation,
    ) -> Result<ObserveOutcome, RecognitionError> {
        self.stats.received = self.stats.received.saturating_add(1);
        let result = self.observe_inner(observation);
        match &result {
            Ok(ObserveOutcome::Added) => {
                self.stats.accepted = self.stats.accepted.saturating_add(1)
            }
            Ok(ObserveOutcome::Duplicate) => {
                self.stats.duplicates = self.stats.duplicates.saturating_add(1)
            }
            Err(error) => {
                let excluded = self.stats.exclusions.entry(error.exclusion()).or_default();
                *excluded = excluded.saturating_add(1);
            }
        }
        result
    }

    pub fn stats(&self) -> RecognitionStats {
        RecognitionStats {
            retained_observations: self.observations.len(),
            quarantined_ids: self.quarantined.len(),
            clusters: self.index.len(),
            ..self.stats.clone()
        }
    }

    pub fn retains(&self, observation: &CommandObservation) -> bool {
        self.observations
            .get(&ObservationKey::new(observation))
            .is_some_and(|stored| stored.observation == *observation)
    }

    pub fn suggestions(&self) -> Result<Vec<PatternCandidate>, RecognitionError> {
        let mut candidates = Vec::new();
        for keys in self.index.values() {
            if keys.len() < self.limits.min_support {
                continue;
            }
            let rows = self.rows(keys)?;
            let definition = anti_unify(&rows)?;
            let evidence = evidence(&definition, &rows);
            if evidence.support.independent_sessions < self.limits.min_sessions {
                continue;
            }
            let rank = (
                Reverse(evidence.support.independent_sessions),
                Reverse(evidence.support.observations),
                evidence.provenance.clone(),
                Reverse(evidence.last_seen_ms),
                Reverse(definition.slots.len()),
                definition.fingerprint()?,
            );
            candidates.push((
                rank,
                PatternCandidate {
                    definition,
                    evidence,
                },
            ));
        }
        candidates.sort_by(|(left, _), (right, _)| left.cmp(right));
        Ok(candidates
            .into_iter()
            .take(self.limits.max_suggestions)
            .map(|(_, candidate)| candidate)
            .collect())
    }

    fn rows(
        &self,
        keys: &BTreeSet<ObservationKey>,
    ) -> Result<Vec<&CommandObservation>, RecognitionError> {
        keys.iter()
            .map(|key| {
                self.observations
                    .get(key)
                    .map(|stored| &stored.observation)
                    .ok_or(RecognitionError::IndexInvariant)
            })
            .collect()
    }

    fn observe_inner(
        &mut self,
        observation: CommandObservation,
    ) -> Result<ObserveOutcome, RecognitionError> {
        observation.validate()?;
        if observation.source.timestamp_ms > self.as_of_timestamp_ms {
            return Err(RecognitionError::FutureObservation);
        }
        if observation.source.outcome == InvocationOutcome::Rejected {
            return Err(RecognitionError::RejectedInvocation);
        }
        let key = ObservationKey::new(&observation);
        if self.quarantined.contains(&key) {
            return Err(RecognitionError::Quarantined);
        }
        if let Some(previous) = self.observations.get(&key) {
            if previous.observation == observation {
                return Ok(ObserveOutcome::Duplicate);
            }
            let shape = ShapeKey::new(&previous.observation);
            self.stats.retained_bytes = self.stats.retained_bytes.saturating_sub(previous.bytes);
            self.observations.remove(&key);
            if let Some(keys) = self.index.get_mut(&shape) {
                keys.remove(&key);
                if keys.is_empty() {
                    self.index.remove(&shape);
                }
            }
            self.quarantined.insert(key);
            return Err(RecognitionError::Collision);
        }
        if self.observations.len() + self.quarantined.len() >= self.limits.max_observations {
            return Err(RecognitionError::Capacity("observation IDs"));
        }
        let bytes = serde_json::to_vec(&observation)
            .map_err(ObservationError::from)?
            .len();
        if bytes > MAX_OBSERVATION_JSON_BYTES
            || bytes
                > self
                    .limits
                    .max_observation_bytes
                    .saturating_sub(self.stats.retained_bytes)
        {
            return Err(RecognitionError::Capacity("observation bytes"));
        }
        let shape = ShapeKey::new(&observation);
        let mut rows = if let Some(keys) = self.index.get(&shape) {
            if keys.len() >= self.limits.max_cluster_observations {
                return Err(RecognitionError::Capacity(
                    "observations per structural cluster",
                ));
            }
            self.rows(keys)?
        } else {
            if self.index.len() >= self.limits.max_clusters {
                return Err(RecognitionError::Capacity("structural clusters"));
            }
            Vec::new()
        };
        rows.push(&observation);
        anti_unify(&rows)?;
        self.stats.retained_bytes += bytes;
        self.index.entry(shape).or_default().insert(key.clone());
        self.observations
            .insert(key, StoredObservation { observation, bytes });
        Ok(ObserveOutcome::Added)
    }
}

impl RecognitionError {
    fn exclusion(&self) -> RecognitionExclusion {
        match self {
            Self::Observation(ObservationError::UnverifiedFacts) => {
                RecognitionExclusion::UnverifiedFacts
            }
            Self::Observation(ObservationError::SensitiveOrPayload) => {
                RecognitionExclusion::SensitiveOrPayload
            }
            Self::RejectedInvocation => RecognitionExclusion::RejectedInvocation,
            Self::Collision | Self::Quarantined => RecognitionExclusion::Collision,
            Self::FutureObservation => RecognitionExclusion::FutureTimestamp,
            Self::Capacity(_) | Self::Definition(PatternValidationError::LimitExceeded { .. }) => {
                RecognitionExclusion::Capacity
            }
            _ => RecognitionExclusion::InvalidInput,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommandObservation, InvocationOutcome, MAX_OBSERVATION_JSON_BYTES,
        MAX_RECOGNIZER_OBSERVATIONS, ObservationError, ObservationProvenance, ObserveOutcome,
        PatternCandidate, PatternRecognizer, RecognitionError, RecognitionExclusion,
        RecognizerLimits, ShellEffectStatus,
        fixtures::{NOW_MS, observation},
        index::ShapeKey,
    };
    use crate::permissions::pattern_matching::{CompiledPattern, MatchReport, PatternMismatch};
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, MAX_ARGUMENT_BYTES, MAX_ARGV_BYTES, MAX_PATTERN_SLOTS,
        PatternToken, SlotCombinations, SlotId,
    };
    use std::collections::BTreeSet;
    use test_case::test_case;

    const FIRST: [&str; 4] = ["nimblectl", "inspect", "alpha", "left"];
    const SECOND: [&str; 4] = ["nimblectl", "inspect", "beta", "right"];
    const PERMUTATION_ROWS: usize = 6;
    const REJECTION_STRESS_ROWS: usize = 16;
    const BYTE_LIMIT_ARGV: [&str; 10] = [
        "unlisted-cli",
        "inspect",
        "a",
        "a",
        "a",
        "a",
        "a",
        "a",
        "a",
        "a",
    ];

    fn recognizer() -> PatternRecognizer {
        PatternRecognizer::new(RecognizerLimits::default(), NOW_MS).unwrap()
    }

    fn pair(recognizer: &mut PatternRecognizer) -> PatternCandidate {
        recognizer.observe(observation(&FIRST, "first")).unwrap();
        recognizer.observe(observation(&SECOND, "second")).unwrap();
        recognizer.suggestions().unwrap().remove(0)
    }

    fn retained(learner: &PatternRecognizer) -> Vec<CommandObservation> {
        learner
            .observations
            .values()
            .map(|stored| stored.observation.clone())
            .collect()
    }

    fn assert_caps(learner: &PatternRecognizer) {
        let stats = learner.stats();
        assert!(
            stats.retained_observations + stats.quarantined_ids <= learner.limits.max_observations
        );
        assert!(stats.retained_bytes <= learner.limits.max_observation_bytes);
        assert!(stats.clusters <= learner.limits.max_clusters);
        assert_eq!(
            stats.accepted,
            (stats.retained_observations + stats.quarantined_ids) as u64
        );
        assert_eq!(
            stats.received,
            stats.accepted + stats.duplicates + stats.exclusions.values().sum::<u64>()
        );
        assert_eq!(
            stats.retained_bytes,
            learner
                .observations
                .values()
                .map(|stored| serde_json::to_vec(&stored.observation).unwrap().len())
                .sum::<usize>()
        );
        let mut indexed = BTreeSet::new();
        for (shape, keys) in &learner.index {
            assert!(!keys.is_empty());
            assert!(keys.len() <= learner.limits.max_cluster_observations);
            for key in keys {
                let row = &learner.observations[key].observation;
                assert_eq!(*shape, ShapeKey::new(row));
                assert!(!learner.quarantined.contains(key));
                assert!(learner.retains(row));
                assert!(indexed.insert(key));
            }
        }
        assert!(indexed.into_iter().eq(learner.observations.keys()));
        assert!(learner.suggestions().unwrap().len() <= learner.limits.max_suggestions);
    }

    fn permutations(order: &mut [usize], start: usize, check: &mut impl FnMut(&[usize])) {
        if start == order.len() {
            check(order);
        } else {
            for index in start..order.len() {
                order.swap(start, index);
                permutations(order, start + 1, check);
                order.swap(start, index);
            }
        }
    }

    fn assert_uncapped_permutations(rows: &[CommandObservation]) {
        let mut baseline = recognizer();
        for row in rows {
            baseline.observe(row.clone()).unwrap();
        }
        let expected = retained(&baseline);
        let candidates = baseline.suggestions().unwrap();
        permutations(&mut (0..rows.len()).collect::<Vec<_>>(), 0, &mut |order| {
            let mut learner = recognizer();
            for index in order {
                learner.observe(rows[*index].clone()).unwrap();
                assert_caps(&learner);
            }
            assert_eq!(retained(&learner), expected, "order={order:?}");
            assert_eq!(
                learner.suggestions().unwrap(),
                candidates,
                "order={order:?}"
            );
            assert_eq!(learner.stats(), baseline.stats());
        });
    }

    fn large_observation(id: &str) -> CommandObservation {
        let mut row = observation(&BYTE_LIMIT_ARGV, id);
        let mut remaining =
            MAX_ARGV_BYTES - 1 - row.argv[..2].iter().map(String::len).sum::<usize>();
        for value in &mut row.argv[2..] {
            let bytes = remaining.min(MAX_ARGUMENT_BYTES);
            *value = "x".repeat(bytes);
            remaining -= bytes;
        }
        assert_eq!(remaining, 0);
        row.validate().unwrap();
        row
    }

    #[test_case(false; "uncapped_distinct_observations")]
    #[test_case(true; "uncapped_duplicate_observations")]
    fn uncapped_membership_and_evidence_are_permutation_invariant(duplicate: bool) {
        let mut rows: Vec<_> = (0..PERMUTATION_ROWS)
            .map(|index| {
                observation(
                    &[
                        FIRST[0],
                        &format!("operation-{}", index / 3),
                        &format!("value-{index}"),
                    ],
                    &format!("row-{index}"),
                )
            })
            .collect();
        if duplicate {
            rows[PERMUTATION_ROWS - 1] = rows[0].clone();
        }
        assert_uncapped_permutations(&rows);
    }

    #[test_case(false; "dense_cluster_forward")]
    #[test_case(true; "dense_cluster_reverse")]
    fn dense_clusters_can_use_the_configured_observation_limit(reverse: bool) {
        let mut learner = recognizer();
        let count = learner.limits.max_cluster_observations;
        let mut rows: Vec<_> = (0..count)
            .map(|index| {
                observation(
                    &[FIRST[0], FIRST[1], &format!("value-{index}")],
                    &format!("row-{index}"),
                )
            })
            .collect();
        if reverse {
            rows.reverse();
        }
        for row in &rows {
            learner.observe(row.clone()).unwrap();
        }
        let before = learner.suggestions().unwrap();
        assert_eq!(learner.stats().retained_observations, count);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].evidence.support.observations, count);
        assert_eq!(before[0].evidence.support.independent_sessions, count);
        assert!(rows.iter().all(|row| learner.retains(row)));
        assert!(matches!(
            learner.observe(observation(&[FIRST[0], FIRST[1], "overflow"], "overflow")),
            Err(RecognitionError::Capacity(_))
        ));
        assert_eq!(learner.suggestions().unwrap(), before);
        assert_caps(&learner);
    }

    #[test_case(2; "large_exact_rows_fit_the_global_byte_budget")]
    fn large_rows_have_no_artificial_shard_byte_limit(count: usize) {
        let mut learner = recognizer();
        for index in 0..count {
            let row = large_observation(&format!("row-{index}"));
            assert!(serde_json::to_vec(&row).unwrap().len() > MAX_ARGV_BYTES);
            learner.observe(row).unwrap();
        }
        assert_eq!(learner.stats().retained_observations, count);
        assert_eq!(
            learner.suggestions().unwrap()[0]
                .evidence
                .support
                .observations,
            count
        );
        assert_caps(&learner);
    }

    #[test_case(false, false; "byte_refusals_same_shape")]
    #[test_case(false, true; "byte_refusals_different_shape")]
    #[test_case(true, false; "full_id_capacity_same_shape")]
    #[test_case(true, true; "full_id_capacity_different_shape")]
    fn refused_oversized_new_ids_cannot_erase_viable_evidence(full: bool, change_shape: bool) {
        let limits = RecognizerLimits {
            max_observations: REJECTION_STRESS_ROWS,
            max_observation_bytes: MAX_ARGV_BYTES,
            ..RecognizerLimits::default()
        };
        let mut learner = PatternRecognizer::new(limits, NOW_MS).unwrap();
        let count = if full { REJECTION_STRESS_ROWS } else { 2 };
        let rows: Vec<_> = (0..count)
            .map(|index| observation(&BYTE_LIMIT_ARGV, &format!("retained-{index}")))
            .collect();
        for row in &rows {
            learner.observe(row.clone()).unwrap();
        }
        let before = learner.suggestions().unwrap();
        let before_bytes = learner.stats().retained_bytes;
        for index in 0..REJECTION_STRESS_ROWS {
            let mut row = large_observation(&format!("refused-{index}"));
            if change_shape {
                row.context.effective_workdir = "/another-project".into();
            }
            assert!(serde_json::to_vec(&row).unwrap().len() > learner.limits.max_observation_bytes);
            assert!(matches!(
                learner.observe(row.clone()),
                Err(RecognitionError::Capacity(_))
            ));
            assert!(!learner.retains(&row));
            assert_eq!(learner.stats().retained_bytes, before_bytes);
            assert_eq!(learner.stats().retained_observations, count);
            assert_eq!(learner.stats().clusters, 1);
            assert_eq!(learner.stats().quarantined_ids, 0);
            assert_eq!(learner.suggestions().unwrap(), before);
            assert_caps(&learner);
        }
        assert_eq!(
            learner.stats().exclusions[&RecognitionExclusion::Capacity],
            REJECTION_STRESS_ROWS as u64
        );
        for row in &rows {
            assert!(learner.retains(row));
            assert_eq!(
                learner.observe(row.clone()).unwrap(),
                ObserveOutcome::Duplicate
            );
        }
        if !full {
            let row = observation(&BYTE_LIMIT_ARGV, "refused-0");
            assert_eq!(learner.observe(row.clone()).unwrap(), ObserveOutcome::Added);
            assert!(learner.retains(&row));
        }
        assert_caps(&learner);
    }

    #[test_case("value"; "changed_value_at_full_capacity")]
    #[test_case("shape"; "changed_shape_at_full_capacity")]
    #[test_case("bytes"; "oversized_conflict_at_full_capacity")]
    fn full_capacity_still_deduplicates_and_quarantines_conflicts(change: &str) {
        let first = observation(&BYTE_LIMIT_ARGV, "first");
        let second = observation(&BYTE_LIMIT_ARGV, "second");
        let second_bytes = serde_json::to_vec(&second).unwrap().len();
        let limits = RecognizerLimits {
            max_observations: 2,
            max_clusters: 1,
            max_cluster_observations: 2,
            max_observation_bytes: serde_json::to_vec(&first).unwrap().len() + second_bytes,
            ..RecognizerLimits::default()
        };
        let mut learner = PatternRecognizer::new(limits, NOW_MS).unwrap();
        learner.observe(first.clone()).unwrap();
        learner.observe(second.clone()).unwrap();
        let candidate = learner.suggestions().unwrap().remove(0);
        let frozen = CompiledPattern::compile(&candidate.definition).unwrap();
        assert_eq!(
            learner.observe(first.clone()).unwrap(),
            ObserveOutcome::Duplicate
        );
        let mut conflicting = first.clone();
        match change {
            "value" => conflicting.argv[2] = "conflicting".into(),
            "shape" => conflicting.context.effective_workdir = "/another-project".into(),
            "bytes" => conflicting = large_observation(&first.source.observation_id),
            _ => unreachable!(),
        }
        assert!(matches!(
            learner.observe(conflicting.clone()),
            Err(RecognitionError::Collision)
        ));
        assert_eq!(learner.stats().retained_bytes, second_bytes);
        assert_eq!(learner.stats().retained_observations, 1);
        assert_eq!(learner.stats().quarantined_ids, 1);
        assert!(!learner.retains(&first));
        assert!(!learner.retains(&conflicting));
        assert!(learner.retains(&second));
        assert!(matches!(
            learner.observe(first.clone()),
            Err(RecognitionError::Quarantined)
        ));
        assert!(matches!(
            learner.observe(conflicting.clone()),
            Err(RecognitionError::Quarantined)
        ));
        assert!(matches!(
            learner.observe(observation(&BYTE_LIMIT_ARGV, "new")),
            Err(RecognitionError::Capacity(_))
        ));
        assert!(learner.suggestions().unwrap().is_empty());
        assert_eq!(frozen.definition(), &candidate.definition);
        assert!(frozen.matches(&first).unwrap().is_match());
        assert!(!frozen.matches(&conflicting).unwrap().is_match());
        assert_caps(&learner);
    }

    #[test_case(2; "capacity_refused_matches_are_not_retained_provenance")]
    fn retained_membership_is_not_a_pattern_match(cap: usize) {
        let rows = ["first", "second", "third"].map(|id| observation(&FIRST, id));
        let mut learner = PatternRecognizer::new(
            RecognizerLimits {
                max_cluster_observations: cap,
                ..RecognizerLimits::default()
            },
            NOW_MS,
        )
        .unwrap();
        for row in &rows[..cap] {
            learner.observe(row.clone()).unwrap();
        }
        assert!(matches!(
            learner.observe(rows[cap].clone()),
            Err(RecognitionError::Capacity(_))
        ));
        let compiled =
            CompiledPattern::compile(&learner.suggestions().unwrap()[0].definition).unwrap();
        assert!(compiled.matches(&rows[cap]).unwrap().is_match());
        assert!(!learner.retains(&rows[cap]));
        assert!(rows[..cap].iter().all(|row| learner.retains(row)));
        assert_caps(&learner);
    }

    fn flag_first(argv: &[&str], id: &str) -> CommandObservation {
        let mut row = observation(argv, id);
        row.roles[1] = ArgumentRole::Flag;
        row
    }

    #[test_case("alpha", "left", "beta", "right"; "unknown_search_like_cli")]
    #[test_case("10", "first.txt", "20", "second.txt"; "unknown_count_like_cli")]
    fn flag_first_unknowns_only_admit_observed_joint_tuples(
        first_left: &str,
        first_right: &str,
        second_left: &str,
        second_right: &str,
    ) {
        const EXECUTABLE: &str = "unlisted-cli";
        const FLAG: &str = "-n";
        let rows = [
            flag_first(&[EXECUTABLE, FLAG, first_left, first_right], "first"),
            flag_first(&[EXECUTABLE, FLAG, second_left, second_right], "second"),
        ];
        let mut learner = recognizer();
        for row in &rows {
            learner.observe(row.clone()).unwrap();
        }
        let candidate = learner.suggestions().unwrap().remove(0);
        assert_eq!(candidate.definition.slots.len(), 2);
        assert!(candidate.definition.argv[2..].iter().all(|token| matches!(
            token,
            PatternToken::Slot {
                role: ArgumentRole::Unknown,
                ..
            }
        )));
        assert!(
            candidate
                .definition
                .slots
                .iter()
                .all(|slot| matches!(slot.domain, ArgumentDomain::ObservedSet { .. }))
        );
        let compiled = CompiledPattern::compile(&candidate.definition).unwrap();
        for left in [first_left, second_left, "unseen", "--new-flag"] {
            for right in [first_right, second_right, "unseen", "--new-flag"] {
                let row = flag_first(&[EXECUTABLE, FLAG, left, right], "probe");
                let expected = (left == first_left && right == first_right)
                    || (left == second_left && right == second_right);
                assert_eq!(compiled.matches(&row).unwrap().is_match(), expected);
            }
        }
        assert_uncapped_permutations(&rows);
    }

    #[test_case("executable"; "fixed_executable")]
    #[test_case("flag"; "fixed_flag")]
    #[test_case("extra"; "extra_argv")]
    #[test_case("missing"; "missing_argv")]
    #[test_case("swap"; "position_is_not_a_bag")]
    #[test_case("tuple"; "unobserved_cartesian_pair")]
    #[test_case("value"; "unobserved_literal")]
    #[test_case("option"; "option_like_operand")]
    #[test_case("role"; "unknown_is_not_data")]
    #[test_case("operation"; "operation_is_not_unknown")]
    #[test_case("tool"; "tool_identity")]
    #[test_case("identity"; "executable_identity")]
    #[test_case("cwd"; "effective_workdir")]
    #[test_case("binding"; "path_binding")]
    #[test_case("analysis"; "analysis_version")]
    #[test_case("leading"; "leading_operation")]
    #[test_case("terminator"; "flag_is_not_terminator")]
    #[test_case("moved"; "moved_flag")]
    fn flag_first_near_miss_matrix(change: &str) {
        const ARGV: [&str; 4] = ["unlisted-cli", "-n", "alpha", "left"];
        let mut learner = recognizer();
        learner.observe(flag_first(&ARGV, "first")).unwrap();
        learner
            .observe(flag_first(&[ARGV[0], ARGV[1], "beta", "right"], "second"))
            .unwrap();
        let compiled =
            CompiledPattern::compile(&learner.suggestions().unwrap()[0].definition).unwrap();
        let mut row = flag_first(&ARGV, "probe");
        match change {
            "executable" => row.argv[0] = "another-cli".into(),
            "flag" => row.argv[1] = "--other".into(),
            "extra" => {
                row.argv.push("extra".into());
                row.roles.push(ArgumentRole::Unknown);
            }
            "missing" => {
                row.argv.pop();
                row.roles.pop();
            }
            "swap" => row.argv.swap(2, 3),
            "tuple" => row.argv[3] = "right".into(),
            "value" => row.argv[2] = "unseen".into(),
            "option" => row.argv[2] = "--other".into(),
            "role" => row.roles[2] = ArgumentRole::Data,
            "operation" => row.roles[2] = ArgumentRole::Operation,
            "tool" => row.context.tool_identity.push_str("-other"),
            "identity" => row.context.executable_identity.push_str("-other"),
            "cwd" => row.context.effective_workdir.push_str("-other"),
            "binding" => row.context.path_binding.push_str("-other"),
            "analysis" => row.context.analysis_version.push_str("-other"),
            "leading" => {
                row.argv.insert(1, "inspect".into());
                row.roles.insert(1, ArgumentRole::Operation);
            }
            "terminator" => {
                row.argv[1] = "--".into();
                row.roles[1] = ArgumentRole::OptionTerminator;
            }
            "moved" => {
                row.argv.swap(1, 2);
                row.roles.swap(1, 2);
            }
            _ => unreachable!(),
        }
        assert!(!compiled.matches(&row).unwrap().is_match());
    }

    #[test_case(2; "operation_immediately_after_flag")]
    #[test_case(3; "operation_after_unknown_flag_value")]
    fn flag_first_never_generalizes_actual_operations(operation: usize) {
        let mut rows = [
            flag_first(&["unlisted-cli", "--root", "alpha", "inspect"], "first"),
            flag_first(&["unlisted-cli", "--root", "beta", "mutate"], "second"),
        ];
        let mut learner = recognizer();
        for row in &mut rows {
            row.roles[operation] = ArgumentRole::Operation;
            learner.observe(row.clone()).unwrap();
        }
        assert!(learner.suggestions().unwrap().is_empty());
        assert_eq!(learner.stats().clusters, 2);
        assert_uncapped_permutations(&rows);
    }

    #[test_case(false; "forward_tied_ranking")]
    #[test_case(true; "reverse_tied_ranking")]
    fn tied_candidate_ranks_use_fingerprints(reverse: bool) {
        let mut rows: Vec<_> = (0..PERMUTATION_ROWS)
            .map(|index| {
                observation(
                    &[FIRST[0], &format!("operation-{}", index / 2), "value"],
                    &format!("row-{index}"),
                )
            })
            .collect();
        let mut unlimited = recognizer();
        for row in &rows {
            unlimited.observe(row.clone()).unwrap();
        }
        let mut expected = unlimited.suggestions().unwrap();
        expected.sort_by_key(|candidate| candidate.definition.fingerprint().unwrap());
        if reverse {
            rows.reverse();
        }
        let mut limited = PatternRecognizer::new(
            RecognizerLimits {
                max_suggestions: 1,
                ..RecognizerLimits::default()
            },
            NOW_MS,
        )
        .unwrap();
        for row in rows {
            limited.observe(row).unwrap();
        }
        assert_eq!(limited.suggestions().unwrap(), expected[..1]);
    }

    #[test_case("future", RecognitionExclusion::FutureTimestamp; "future_cannot_evict")]
    #[test_case("rejected", RecognitionExclusion::RejectedInvocation; "rejection_cannot_evict")]
    #[test_case("sensitive", RecognitionExclusion::SensitiveOrPayload; "sensitivity_cannot_evict")]
    #[test_case("payload", RecognitionExclusion::SensitiveOrPayload; "payload_cannot_evict")]
    #[test_case("dynamic", RecognitionExclusion::UnverifiedFacts; "dynamic_cannot_evict")]
    #[test_case("effects", RecognitionExclusion::UnverifiedFacts; "effects_cannot_evict")]
    fn invalid_collision_facts_never_affect_a_full_recognizer(
        field: &str,
        exclusion: RecognitionExclusion,
    ) {
        let mut learner = PatternRecognizer::new(
            RecognizerLimits {
                max_observations: 2,
                max_clusters: 1,
                ..RecognizerLimits::default()
            },
            NOW_MS,
        )
        .unwrap();
        let before = pair(&mut learner);
        let mut row = observation(&FIRST, "first");
        match field {
            "future" => row.source.timestamp_ms = NOW_MS + 1,
            "rejected" => row.source.outcome = InvocationOutcome::Rejected,
            "sensitive" => row.roles[2] = ArgumentRole::Sensitive,
            "payload" => row.roles[2] = ArgumentRole::Payload,
            "dynamic" => row.verification.static_argv = false,
            "effects" => row.verification.shell_effects = ShellEffectStatus::Present,
            _ => unreachable!(),
        }
        assert!(learner.observe(row).is_err());
        assert_eq!(learner.suggestions().unwrap(), [before]);
        assert_eq!(learner.stats().exclusions[&exclusion], 1);
        assert_eq!(learner.stats().quarantined_ids, 0);
        assert_caps(&learner);
    }

    #[test_case(InvocationOutcome::Failed; "failures_remain_labeled_not_successes")]
    #[test_case(InvocationOutcome::Unknown; "imports_remain_unknown")]
    fn outcomes_and_source_scoped_sessions_are_preserved(outcome: InvocationOutcome) {
        let mut learner = recognizer();
        for (index, argv) in [FIRST, SECOND].iter().enumerate() {
            let mut row = observation(argv, "same-id");
            row.source.source_identity = format!("source-{index}");
            row.source.outcome = outcome.clone();
            learner.observe(row).unwrap();
        }
        let candidate = learner.suggestions().unwrap().remove(0);
        assert_eq!(candidate.evidence.support.independent_sessions, 2);
        assert_eq!(candidate.evidence.sources.len(), 2);
        assert_eq!(candidate.evidence.outcomes, [(outcome, 2)].into());
        assert_caps(&learner);
    }

    #[test_case("regex"; "regex_requires_combination_approval_too")]
    #[test_case("any"; "any_literal_requires_combination_approval_too")]
    fn widening_unknown_slots_is_an_explicit_definition_edit(domain: &str) {
        let mut learner = recognizer();
        for (id, value) in [("first", "alpha"), ("second", "beta")] {
            learner
                .observe(flag_first(&["unlisted-cli", "-n", value], id))
                .unwrap();
        }
        let mut candidate = learner.suggestions().unwrap().remove(0);
        let frozen = CompiledPattern::compile(&candidate.definition).unwrap();
        candidate.definition.slots[0].domain = match domain {
            "regex" => ArgumentDomain::Regex {
                pattern: "[a-z]+".into(),
            },
            "any" => ArgumentDomain::AnyLiteralArgument,
            _ => unreachable!(),
        };
        let row = flag_first(&["unlisted-cli", "-n", "gamma"], "probe");
        assert!(!frozen.matches(&row).unwrap().is_match());
        assert!(
            !CompiledPattern::compile(&candidate.definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
        candidate.definition.combinations = SlotCombinations::Independent;
        assert!(
            CompiledPattern::compile(&candidate.definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
        assert!(!frozen.matches(&row).unwrap().is_match());
    }

    #[test_case(1, true; "runtime_can_opt_into_one_session")]
    #[test_case(2, false; "history_keeps_independent_session_requirement")]
    fn one_session_learning_requires_explicit_limits(min_sessions: usize, suggests: bool) {
        let mut learner = PatternRecognizer::new(
            RecognizerLimits {
                min_support: 3,
                min_sessions,
                ..RecognizerLimits::default()
            },
            NOW_MS,
        )
        .unwrap();
        for (index, argv) in [FIRST, SECOND, FIRST].iter().enumerate() {
            let mut row = observation(argv, &index.to_string());
            row.source.session_id = "same-session".into();
            learner.observe(row).unwrap();
            if index < 2 {
                assert!(learner.suggestions().unwrap().is_empty());
            }
        }
        assert_eq!(!learner.suggestions().unwrap().is_empty(), suggests);
        assert_eq!(RecognizerLimits::default().min_sessions, 2);
        assert!(
            PatternRecognizer::new(
                RecognizerLimits {
                    min_sessions: 0,
                    ..RecognizerLimits::default()
                },
                NOW_MS
            )
            .is_err()
        );
    }

    #[test_case(true; "unknown_cli_observed_tuples")]
    fn unknown_cli_needs_no_dictionary(preserve_tuples: bool) {
        let mut learner = recognizer();
        let candidate = pair(&mut learner);
        assert_eq!(candidate.definition.slots.len(), 2);
        assert_eq!(candidate.definition.slots[0].label, "<pattern1>");
        assert_eq!(candidate.definition.slots[1].label, "<pattern2>");
        assert_eq!(candidate.definition.slots[0].id, SlotId(2));
        assert!(matches!(
            candidate.definition.argv[1],
            PatternToken::Exact { .. }
        ));
        assert!(
            candidate
                .definition
                .slots
                .iter()
                .all(|slot| matches!(slot.domain, ArgumentDomain::ObservedSet { .. }))
        );
        assert_eq!(
            matches!(
                candidate.definition.combinations,
                SlotCombinations::ObservedTuples { .. }
            ),
            preserve_tuples
        );
        assert_eq!(candidate.evidence.support.observations, 2);
        assert_eq!(candidate.evidence.support.independent_sessions, 2);
        assert_eq!(candidate.evidence.tuples.len(), 2);
        assert_eq!(
            candidate.evidence.distributions[&SlotId(2)]["alpha"].observations,
            1
        );
        let compiled = CompiledPattern::compile(&candidate.definition).unwrap();
        assert!(
            compiled
                .matches(&observation(&FIRST, "matching"))
                .unwrap()
                .is_match()
        );
    }

    #[test_case("alpha", "right"; "first_unobserved_combination")]
    #[test_case("beta", "left"; "second_unobserved_combination")]
    fn observed_pairs_do_not_create_a_cross_product(left: &str, right: &str) {
        let mut candidate = pair(&mut recognizer());
        let row = observation(&[FIRST[0], FIRST[1], left, right], "matching");
        let compiled = CompiledPattern::compile(&candidate.definition).unwrap();
        assert_eq!(
            compiled.matches(&row).unwrap(),
            MatchReport::NotMatched {
                reason: PatternMismatch::UnobservedTuple
            }
        );
        candidate.definition.combinations = SlotCombinations::Independent;
        assert!(
            CompiledPattern::compile(&candidate.definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
        let new_value = observation(&[FIRST[0], FIRST[1], "gamma", right], "new");
        assert!(
            !CompiledPattern::compile(&candidate.definition)
                .unwrap()
                .matches(&new_value)
                .unwrap()
                .is_match()
        );
    }

    #[test_case("gamma"; "learning_cannot_modify_approval")]
    fn later_observations_never_mutate_a_frozen_matcher(value: &str) {
        let mut learner = recognizer();
        let candidate = pair(&mut learner);
        let compiled = CompiledPattern::compile(&candidate.definition).unwrap();
        let fingerprint = compiled.fingerprint().to_owned();
        let row = observation(&[FIRST[0], FIRST[1], value, "other"], "third");
        learner.observe(row.clone()).unwrap();
        assert!(!compiled.matches(&row).unwrap().is_match());
        assert_eq!(compiled.fingerprint(), fingerprint);
        let proposals = learner.suggestions().unwrap();
        assert_ne!(proposals[0].definition.fingerprint().unwrap(), fingerprint);
        assert!(
            CompiledPattern::compile(&proposals[0].definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
    }

    #[test_case([0, 1, 2]; "original_order")]
    #[test_case([2, 1, 0]; "reverse_order")]
    #[test_case([1, 0, 2]; "swapped_order")]
    #[test_case([2, 0, 1]; "rotated_order")]
    fn discovery_and_evidence_are_order_independent(order: [usize; 3]) {
        let rows = [
            observation(&FIRST, "first"),
            observation(&SECOND, "second"),
            observation(&FIRST, "third"),
        ];
        let mut baseline = recognizer();
        for row in &rows {
            baseline.observe(row.clone()).unwrap();
        }
        let mut permuted = recognizer();
        for index in order {
            permuted.observe(rows[index].clone()).unwrap();
        }
        assert_eq!(
            baseline.suggestions().unwrap(),
            permuted.suggestions().unwrap()
        );
        let candidate = baseline.suggestions().unwrap().remove(0);
        assert_eq!(
            candidate.evidence.distributions[&SlotId(2)]["alpha"].observations,
            2
        );
        assert_eq!(
            candidate.evidence.distributions[&SlotId(2)]["alpha"].independent_sessions,
            2
        );
    }

    #[test_case("operation"; "operation_position_stays_exact")]
    #[test_case("flag"; "flag_names_stay_exact")]
    #[test_case("extra"; "optional_flags_are_separate")]
    #[test_case("context"; "workdirs_are_separate")]
    #[test_case("provenance"; "native_and_legacy_are_separate")]
    fn incompatible_observations_do_not_merge(field: &str) {
        let mut learner = recognizer();
        learner
            .observe(observation(&["cargo", "check", "-p", "alpha"], "first"))
            .unwrap();
        let mut second = observation(&["cargo", "check", "-p", "beta"], "second");
        match field {
            "operation" => second.argv[1] = "push".into(),
            "flag" => second.argv[2] = "--manifest-path".into(),
            "extra" => {
                second.argv.push("--locked".into());
                second.roles.push(ArgumentRole::Unknown);
            }
            "context" => second.context.effective_workdir = "/elsewhere".into(),
            "provenance" => second.source.provenance = ObservationProvenance::Legacy,
            _ => unreachable!(),
        }
        learner.observe(second).unwrap();
        assert!(learner.suggestions().unwrap().is_empty());
        assert_eq!(learner.stats().clusters, 2);
    }

    #[test_case(false; "unknown_leading_flag_arity_abstains")]
    #[test_case(true; "proven_operation_and_data_can_vary")]
    fn leading_flags_do_not_hide_an_operation(proven_roles: bool) {
        let mut learner = recognizer();
        for (id, value) in [("first", "alpha"), ("second", "beta")] {
            let mut row = observation(&[FIRST[0], "--root", "project", "inspect", value], id);
            if proven_roles {
                row.roles[3] = ArgumentRole::Operation;
                row.roles[4] = ArgumentRole::Data;
            }
            learner.observe(row).unwrap();
        }
        assert_eq!(
            learner.suggestions().unwrap().len(),
            usize::from(proven_roles)
        );
    }

    #[test_case(false; "deduplication")]
    #[test_case(true; "collision_quarantines_both_versions")]
    fn repeated_ids_cannot_poison_support(collision: bool) {
        let mut learner = recognizer();
        pair(&mut learner);
        let mut repeated = observation(&FIRST, "first");
        if collision {
            repeated.argv[2] = "poisoned".into();
        }
        let result = learner.observe(repeated.clone());
        if collision {
            assert!(matches!(result, Err(RecognitionError::Collision)));
            assert!(matches!(
                learner.observe(repeated),
                Err(RecognitionError::Quarantined)
            ));
            assert!(learner.suggestions().unwrap().is_empty());
            assert_eq!(learner.stats().retained_observations, 1);
            assert_eq!(learner.stats().quarantined_ids, 1);
            assert_eq!(
                learner.stats().exclusions[&RecognitionExclusion::Collision],
                2
            );
        } else {
            assert_eq!(result.unwrap(), ObserveOutcome::Duplicate);
            assert_eq!(
                learner.suggestions().unwrap()[0]
                    .evidence
                    .support
                    .observations,
                2
            );
            assert_eq!(learner.stats().duplicates, 1);
        }
    }

    #[test_case("same-session"; "single_session_frequency_is_not_independent_support")]
    fn one_session_cannot_manufacture_recurrence(session: &str) {
        let mut learner = recognizer();
        for (id, argv) in [("first", FIRST), ("second", SECOND)] {
            let mut row = observation(&argv, id);
            row.source.session_id = session.into();
            learner.observe(row).unwrap();
        }
        assert!(learner.suggestions().unwrap().is_empty());
    }

    #[test_case("ids"; "observation_cap")]
    #[test_case("cluster"; "cluster_observation_cap")]
    #[test_case("shapes"; "shape_cap")]
    #[test_case("bytes"; "retained_byte_cap")]
    fn capacity_refusals_preserve_existing_candidates(cap: &str) {
        let mut limits = RecognizerLimits::default();
        let first = observation(&FIRST, "first");
        let second = observation(&SECOND, "second");
        match cap {
            "ids" => limits.max_observations = 2,
            "cluster" => limits.max_cluster_observations = 2,
            "shapes" => limits.max_clusters = 1,
            "bytes" => {
                limits.max_observation_bytes = serde_json::to_vec(&first).unwrap().len()
                    + serde_json::to_vec(&second).unwrap().len()
            }
            _ => unreachable!(),
        }
        let mut learner = PatternRecognizer::new(limits, NOW_MS).unwrap();
        learner.observe(first.clone()).unwrap();
        learner.observe(second.clone()).unwrap();
        let before = learner.suggestions().unwrap();
        let before_bytes = learner.stats().retained_bytes;
        let frozen = CompiledPattern::compile(&before[0].definition).unwrap();
        let mut third = observation(&[FIRST[0], FIRST[1], "unseen", "other"], "third");
        if cap == "shapes" {
            third.argv[1] = "other-operation".into();
        }
        assert!(matches!(
            learner.observe(third.clone()),
            Err(RecognitionError::Capacity(_))
        ));
        assert_eq!(learner.suggestions().unwrap(), before);
        assert_eq!(learner.stats().retained_observations, 2);
        assert_eq!(learner.stats().retained_bytes, before_bytes);
        assert_eq!(
            learner.stats().exclusions[&RecognitionExclusion::Capacity],
            1
        );
        assert!(learner.retains(&first));
        assert!(learner.retains(&second));
        assert!(!learner.retains(&third));
        assert_eq!(frozen.definition(), &before[0].definition);
        assert!(!frozen.matches(&third).unwrap().is_match());
        assert_caps(&learner);
    }

    #[test_case(MAX_PATTERN_SLOTS + 1; "slot_cap_does_not_partially_merge")]
    fn excessive_variation_is_transactionally_refused(slots: usize) {
        let mut learner = recognizer();
        let mut first = vec![FIRST[0], FIRST[1]];
        first.extend(vec!["alpha"; slots]);
        let mut second = vec![FIRST[0], FIRST[1]];
        second.extend(vec!["beta"; slots]);
        learner.observe(observation(&first, "first")).unwrap();
        assert!(matches!(
            learner.observe(observation(&second, "second")),
            Err(RecognitionError::Definition(_))
        ));
        assert_eq!(learner.stats().retained_observations, 1);
        assert!(learner.suggestions().unwrap().is_empty());
        assert_caps(&learner);
    }

    #[test_case(6; "definition_byte_limit_does_not_partially_merge")]
    fn oversized_definitions_are_transactionally_refused(slots: usize) {
        let mut rows = Vec::new();
        for (id, value) in [("large-first", 'a'), ("large-second", 'b')] {
            let value = value.to_string().repeat(MAX_ARGUMENT_BYTES);
            let mut argv = vec![FIRST[0], FIRST[1]];
            argv.extend(vec![value.as_str(); slots]);
            rows.push(observation(&argv, id));
        }
        let mut learner = recognizer();
        learner.observe(rows[0].clone()).unwrap();
        let before_bytes = learner.stats().retained_bytes;
        assert!(matches!(
            learner.observe(rows[1].clone()),
            Err(RecognitionError::Definition(_))
        ));
        assert_eq!(learner.stats().retained_bytes, before_bytes);
        assert_eq!(learner.stats().retained_observations, 1);
        assert!(learner.retains(&rows[0]));
        assert!(!learner.retains(&rows[1]));
        pair(&mut learner);
        assert_eq!(learner.stats().retained_observations, 3);
        assert_eq!(learner.suggestions().unwrap().len(), 1);
        assert_caps(&learner);
    }

    #[test_case(0; "zero_limit")]
    #[test_case(MAX_RECOGNIZER_OBSERVATIONS + 1; "limit_above_hard_cap")]
    fn configured_limits_cannot_disable_hard_bounds(max_observations: usize) {
        let limits = RecognizerLimits {
            max_observations,
            ..RecognizerLimits::default()
        };
        assert!(matches!(
            PatternRecognizer::new(limits, NOW_MS),
            Err(RecognitionError::InvalidLimits)
        ));
    }

    #[test_case("sensitive"; "sensitive_exclusion")]
    #[test_case("payload"; "payload_exclusion")]
    #[test_case("opaque"; "opaque_exclusion")]
    #[test_case("effects"; "unknown_effects_exclusion")]
    #[test_case("future"; "fixed_corpus_cutoff")]
    #[test_case("rejected"; "rejection_is_not_positive_evidence")]
    fn rejected_evidence_never_enters_the_index(field: &str) {
        let mut learner = recognizer();
        let mut row = observation(&FIRST, "first");
        match field {
            "sensitive" => row.roles[2] = ArgumentRole::Sensitive,
            "payload" => row.roles[2] = ArgumentRole::Payload,
            "opaque" => row.verification.complete_command = false,
            "effects" => row.verification.shell_effects = ShellEffectStatus::Unknown,
            "future" => row.source.timestamp_ms = NOW_MS + 1,
            "rejected" => row.source.outcome = InvocationOutcome::Rejected,
            _ => unreachable!(),
        }
        assert!(learner.observe(row).is_err());
        assert_eq!(learner.stats().retained_observations, 0);
        assert_eq!(learner.stats().exclusions.values().sum::<u64>(), 1);
    }

    #[test_case("hash"; "malformed_input_hash")]
    #[test_case("timestamp"; "invalid_timestamp")]
    #[test_case("identity"; "missing_identity")]
    #[test_case("role"; "role_count_mismatch")]
    #[test_case("version"; "future_observation_version")]
    #[test_case("unknown"; "unknown_json_field")]
    #[test_case("nul"; "nul_is_not_an_argv_value")]
    fn corpus_input_requires_the_exact_typed_contract(field: &str) {
        let mut json = serde_json::to_value(observation(&FIRST, "first")).unwrap();
        match field {
            "hash" => json["source"]["input_hash"] = "not-a-hash".into(),
            "timestamp" => json["source"]["timestamp_ms"] = 0.into(),
            "identity" => json["source"]["source_identity"] = "".into(),
            "role" => json["roles"] = serde_json::json!([]),
            "version" => json["version"] = 2.into(),
            "unknown" => json["eval"] = "do not run".into(),
            "nul" => json["argv"][2] = "\0".into(),
            _ => unreachable!(),
        }
        assert!(CommandObservation::from_json(&json.to_string()).is_err());
        assert!(matches!(
            CommandObservation::from_json(&" ".repeat(MAX_OBSERVATION_JSON_BYTES + 1)),
            Err(ObservationError::JsonLimit)
        ));
    }

    #[test_case("sessions"; "independent_sessions_rank_first")]
    #[test_case("support"; "request_support_ranks_next")]
    #[test_case("provenance"; "provenance_is_not_pooled")]
    #[test_case("recency"; "fixed_timestamps_rank_recency")]
    #[test_case("variation"; "reducible_variation_breaks_ties")]
    fn ranking_is_deterministic_and_suggestions_are_bounded(criterion: &str) {
        let limits = RecognizerLimits {
            max_suggestions: 1,
            ..RecognizerLimits::default()
        };
        let mut learner = PatternRecognizer::new(limits, NOW_MS).unwrap();
        for preferred in [false, true] {
            let operation = if preferred {
                "preferred"
            } else {
                "alternative"
            };
            let count = match (criterion, preferred) {
                ("sessions", true) | ("support", true) => 3,
                ("sessions", false) => 4,
                _ => 2,
            };
            for index in 0..count {
                let id = format!("{operation}-{index}");
                let value = if index == 0 || (criterion == "variation" && !preferred) {
                    "alpha"
                } else {
                    "beta"
                };
                let mut row = observation(&[FIRST[0], operation, value], &id);
                if (criterion == "sessions" && !preferred) || criterion == "support" {
                    row.source.session_id = format!("session-{}", index % 2);
                }
                if criterion == "provenance" && !preferred {
                    row.source.provenance = ObservationProvenance::Legacy;
                }
                if (criterion == "provenance" && preferred)
                    || (criterion == "recency" && !preferred)
                {
                    row.source.timestamp_ms = 1;
                }
                learner.observe(row).unwrap();
            }
        }
        let suggestions = learner.suggestions().unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(learner.stats().clusters, 2);
        assert!(
            matches!(&suggestions[0].definition.argv[1], PatternToken::Exact { value, .. } if value == "preferred")
        );
    }
}
