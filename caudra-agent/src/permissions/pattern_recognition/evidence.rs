use caudra_storage::permission_patterns::{ObservedTuple, PatternDefinition, SlotId};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use super::{CommandObservation, InvocationOutcome, ObservationProvenance};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SupportCount {
    pub observations: usize,
    pub independent_sessions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TupleSupport {
    pub values: ObservedTuple,
    pub support: SupportCount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateEvidence {
    pub support: SupportCount,
    pub provenance: ObservationProvenance,
    pub sources: BTreeSet<String>,
    pub outcomes: BTreeMap<InvocationOutcome, usize>,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub distributions: BTreeMap<SlotId, BTreeMap<String, SupportCount>>,
    pub tuples: Vec<TupleSupport>,
}

#[derive(Default)]
struct Counts<'a> {
    observations: usize,
    sessions: BTreeSet<(&'a str, &'a str)>,
}

impl<'a> Counts<'a> {
    fn observe(&mut self, row: &'a CommandObservation) {
        self.observations += 1;
        self.sessions
            .insert((&row.source.source_identity, &row.source.session_id));
    }

    fn finish(self) -> SupportCount {
        SupportCount {
            observations: self.observations,
            independent_sessions: self.sessions.len(),
        }
    }
}

pub(super) fn evidence(
    definition: &PatternDefinition,
    rows: &[&CommandObservation],
) -> CandidateEvidence {
    let mut support = Counts::default();
    let mut sources = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
    let mut distributions: BTreeMap<SlotId, BTreeMap<&str, Counts<'_>>> = BTreeMap::new();
    let mut tuples: BTreeMap<ObservedTuple, Counts<'_>> = BTreeMap::new();
    for row in rows {
        support.observe(row);
        sources.insert(row.source.source_identity.clone());
        *outcomes.entry(row.source.outcome.clone()).or_default() += 1;
        let mut tuple = BTreeMap::new();
        for slot in &definition.slots {
            let value = &row.argv[usize::from(slot.id.0)];
            distributions
                .entry(slot.id)
                .or_default()
                .entry(value)
                .or_default()
                .observe(row);
            tuple.insert(slot.id, value.clone());
        }
        tuples.entry(tuple).or_default().observe(row);
    }
    CandidateEvidence {
        support: support.finish(),
        provenance: rows
            .first()
            .map(|row| row.source.provenance.clone())
            .unwrap_or(ObservationProvenance::Unknown),
        sources,
        outcomes,
        first_seen_ms: rows
            .iter()
            .map(|row| row.source.timestamp_ms)
            .min()
            .unwrap_or_default(),
        last_seen_ms: rows
            .iter()
            .map(|row| row.source.timestamp_ms)
            .max()
            .unwrap_or_default(),
        distributions: distributions
            .into_iter()
            .map(|(id, values)| {
                (
                    id,
                    values
                        .into_iter()
                        .map(|(value, support)| (value.into(), support.finish()))
                        .collect(),
                )
            })
            .collect(),
        tuples: tuples
            .into_iter()
            .map(|(values, support)| TupleSupport {
                values,
                support: support.finish(),
            })
            .collect(),
    }
}
