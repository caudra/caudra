//! Assembles per-file extraction into one fact table.
//!
//! The determinism contract starts here. Output is a sorted ranking, and a sort has no tolerance
//! band, so the same tree must produce the same bytes on every run. Two rules hold up this stage:
//!
//! 1. **Paths are sorted by byte before any id is assigned.** `FileId` is an index into that sorted
//!    list, so it does not depend on directory iteration order, which no filesystem guarantees.
//! 2. **`NodeId` is assigned in `(file, span start, name start)` order** after every file has been
//!    extracted and merged. Collection order never reaches the output, which is what makes it safe
//!    to parse files in any order or concurrently.
//!
//! Neither rule is expensive, and both are invisible until someone runs the same map on two
//! machines and diffs it.

use workcell_source_languages::Language;

use crate::{
    extract::{ExtractLimits, FileFacts, extract},
    model::{Definition, Facts, FileId, NodeId, Reference, SkipReason, SkippedFile, SourceFile},
};

/// One file offered to the crawl.
///
/// The caller has already resolved, authorized, and read it. This crate never opens a path.
#[derive(Clone, Debug)]
pub struct SourceInput {
    /// Root-relative, forward-slash separated.
    pub path: String,
    pub source: String,
}

/// Ingest limits. Host-owned; never accepted from model input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngestLimits {
    pub extract: ExtractLimits,
    /// Most files admitted to one map.
    pub max_files: usize,
    /// Most definitions retained across the whole tree.
    pub max_definitions: usize,
    /// Most references retained across the whole tree.
    pub max_references: usize,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            extract: ExtractLimits::default(),
            max_files: 25_000,
            max_definitions: 400_000,
            max_references: 1_500_000,
        }
    }
}

/// What ingest had to leave out, so the caller can disclose it rather than imply completeness.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IngestTruncation {
    pub files: bool,
    pub definitions: bool,
    pub references: bool,
}

impl IngestTruncation {
    #[must_use]
    pub const fn any(self) -> bool {
        self.files || self.definitions || self.references
    }
}

/// The result of ingesting a tree.
#[derive(Clone, Debug, Default)]
pub struct Ingested {
    pub facts: Facts,
    pub truncation: IngestTruncation,
}

/// Builds the fact table for a set of already-read sources.
///
/// Inputs may arrive in any order; this sorts them. Duplicated paths are resolved by keeping the
/// first after sorting, so a caller that offers the same path twice cannot mint two file ids.
#[must_use]
pub fn ingest(mut inputs: Vec<SourceInput>, limits: IngestLimits) -> Ingested {
    inputs.sort_by(|left, right| left.path.cmp(&right.path));
    inputs.dedup_by(|left, right| left.path == right.path);

    let mut truncation = IngestTruncation::default();
    if inputs.len() > limits.max_files {
        truncation.files = true;
        inputs.truncate(limits.max_files);
    }

    let mut facts = Facts::default();
    // Per-file extraction results, retained only until ids are assigned. The tree itself is already
    // gone: `extract` drops it before returning.
    let mut per_file: Vec<(FileId, FileFacts)> = Vec::with_capacity(inputs.len());

    for input in inputs {
        let path = input.path;
        let Some(language) = Language::from_path(std::path::Path::new(&path)) else {
            facts.skipped.push(SkippedFile {
                extension: extension_of(&path),
                path,
                reason: SkipReason::UnknownLanguage,
            });
            continue;
        };
        if input.source.len() > limits.extract.max_source_bytes {
            facts.skipped.push(SkippedFile {
                extension: extension_of(&path),
                path,
                reason: SkipReason::Oversize,
            });
            continue;
        }
        let Some(file_facts) = extract(&input.source, language, limits.extract) else {
            facts.skipped.push(SkippedFile {
                extension: extension_of(&path),
                path,
                reason: SkipReason::ParserTimeout,
            });
            continue;
        };

        let id = FileId::try_from(facts.files.len()).unwrap_or(FileId::MAX);
        if file_facts.parse_error {
            facts.files_with_parse_errors += 1;
        }
        truncation.definitions |= file_facts.definitions_truncated;
        truncation.references |= file_facts.references_truncated;
        facts.files.push(SourceFile {
            id,
            path,
            language,
            bytes: input.source.len(),
            lines: input.source.lines().count(),
        });
        per_file.push((id, file_facts));
    }

    // Ids are assigned here, in file order and then span order, never during extraction.
    let mut first_node_of_file: Vec<NodeId> = Vec::with_capacity(per_file.len());
    for (file, file_facts) in &per_file {
        first_node_of_file.push(NodeId::try_from(facts.definitions.len()).unwrap_or(NodeId::MAX));
        for raw in &file_facts.definitions {
            if facts.definitions.len() >= limits.max_definitions {
                truncation.definitions = true;
                break;
            }
            let node = NodeId::try_from(facts.definitions.len()).unwrap_or(NodeId::MAX);
            facts.definitions.push(Definition {
                node,
                file: *file,
                name: raw.name.clone(),
                kind: raw.kind,
                name_start: raw.name_start,
                span: raw.span,
                lines: raw.lines,
                metrics: raw.metrics,
                test_scope: raw.test_scope,
                documentation: raw.documentation.clone(),
            });
        }
    }

    for (index, (file, file_facts)) in per_file.iter().enumerate() {
        let base = first_node_of_file[index];
        let count = facts
            .definitions
            .iter()
            .skip(base as usize)
            .take_while(|definition| definition.file == *file)
            .count();
        for raw in &file_facts.references {
            if facts.references.len() >= limits.max_references {
                truncation.references = true;
                break;
            }
            // A reference whose enclosing definition was dropped by the definition ceiling loses
            // its attribution rather than pointing at an unrelated symbol.
            let enclosing = raw
                .enclosing
                .filter(|&local| local < count)
                .and_then(|local| NodeId::try_from(base as usize + local).ok());
            facts.references.push(Reference {
                file: *file,
                name: raw.name.clone(),
                kind: raw.kind,
                qualifier: raw.qualifier.clone(),
                byte: raw.byte,
                line: raw.line,
                enclosing,
            });
        }
    }

    Ingested { facts, truncation }
}

fn extension_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (_, extension) = name.rsplit_once('.')?;
    if extension.is_empty() || extension.len() > 16 {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str, source: &str) -> SourceInput {
        SourceInput {
            path: path.to_owned(),
            source: source.to_owned(),
        }
    }

    #[test]
    fn file_ids_follow_sorted_paths_not_input_order() {
        // The rule that makes a map reproducible across machines: no filesystem guarantees
        // directory iteration order, so ids must come from a sort the crawl controls.
        let forward = ingest(
            vec![
                input("a/one.rs", "fn one() {}"),
                input("b/two.rs", "fn two() {}"),
                input("c/three.rs", "fn three() {}"),
            ],
            IngestLimits::default(),
        );
        let shuffled = ingest(
            vec![
                input("c/three.rs", "fn three() {}"),
                input("a/one.rs", "fn one() {}"),
                input("b/two.rs", "fn two() {}"),
            ],
            IngestLimits::default(),
        );
        let paths = |ingested: &Ingested| {
            ingested
                .facts
                .files
                .iter()
                .map(|file| (file.id, file.path.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&forward), paths(&shuffled));
        assert_eq!(forward.facts.files[0].path, "a/one.rs");
    }

    #[test]
    fn node_ids_are_dense_and_ordered_by_file_then_span() {
        let ingested = ingest(
            vec![
                input("a.rs", "fn first() {}\nfn second() {}"),
                input("b.rs", "fn third() {}"),
            ],
            IngestLimits::default(),
        );
        for (index, definition) in ingested.facts.definitions.iter().enumerate() {
            assert_eq!(definition.node as usize, index);
        }
        let order: Vec<_> = ingested
            .facts
            .definitions
            .iter()
            .map(|definition| (definition.file, definition.span.start))
            .collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted);
    }

    #[test]
    fn a_reference_points_at_the_global_id_of_its_enclosing_definition() {
        let ingested = ingest(
            vec![
                input("a.rs", "fn alpha() {}"),
                input("b.rs", "fn beta() { alpha(); }"),
            ],
            IngestLimits::default(),
        );
        let call = ingested
            .facts
            .references
            .iter()
            .find(|reference| reference.name == "alpha")
            .expect("call");
        let enclosing = call.enclosing.expect("attributed");
        let definition = ingested.facts.definition(enclosing).expect("definition");
        assert_eq!(definition.name, "beta");
        assert_eq!(ingested.facts.path_of(enclosing), Some("b.rs"));
    }

    #[test]
    fn an_unknown_extension_is_recorded_as_a_skip_not_dropped() {
        let ingested = ingest(
            vec![
                input("model.ml", "let f x = x"),
                input("keep.rs", "fn keep() {}"),
            ],
            IngestLimits::default(),
        );
        assert_eq!(ingested.facts.files.len(), 1);
        assert_eq!(ingested.facts.skipped.len(), 1);
        assert_eq!(
            ingested.facts.skipped[0].reason,
            SkipReason::UnknownLanguage
        );
        assert_eq!(ingested.facts.skipped[0].extension.as_deref(), Some("ml"));
    }

    #[test]
    fn an_oversize_file_is_skipped_with_its_own_reason() {
        let limits = IngestLimits {
            extract: ExtractLimits {
                max_source_bytes: 8,
                ..ExtractLimits::default()
            },
            ..IngestLimits::default()
        };
        let ingested = ingest(
            vec![input("big.rs", "fn a_long_function_name() {}")],
            limits,
        );
        assert!(ingested.facts.files.is_empty());
        assert_eq!(ingested.facts.skipped[0].reason, SkipReason::Oversize);
    }

    #[test]
    fn duplicate_paths_cannot_mint_two_file_ids() {
        let ingested = ingest(
            vec![input("a.rs", "fn one() {}"), input("a.rs", "fn two() {}")],
            IngestLimits::default(),
        );
        assert_eq!(ingested.facts.files.len(), 1);
    }

    #[test]
    fn exceeding_the_file_ceiling_truncates_and_says_so() {
        let limits = IngestLimits {
            max_files: 1,
            ..IngestLimits::default()
        };
        let ingested = ingest(
            vec![input("a.rs", "fn a() {}"), input("b.rs", "fn b() {}")],
            limits,
        );
        assert!(ingested.truncation.files);
        assert_eq!(ingested.facts.files.len(), 1);
    }

    #[test]
    fn ingest_is_byte_identical_across_repeated_runs() {
        let inputs = || {
            vec![
                input("src/lib.rs", include_str!("lib.rs")),
                input("src/model.rs", include_str!("model.rs")),
                input("src/ingest.rs", include_str!("ingest.rs")),
            ]
        };
        let first = ingest(inputs(), IngestLimits::default());
        let second = ingest(inputs(), IngestLimits::default());
        let shape = |ingested: &Ingested| {
            format!(
                "{:?}|{:?}",
                ingested
                    .facts
                    .definitions
                    .iter()
                    .map(|definition| (
                        definition.node,
                        definition.file,
                        definition.name.as_str(),
                        definition.kind,
                        definition.span.start,
                    ))
                    .collect::<Vec<_>>(),
                ingested
                    .facts
                    .references
                    .iter()
                    .map(|reference| (
                        reference.file,
                        reference.name.as_str(),
                        reference.byte,
                        reference.enclosing
                    ))
                    .collect::<Vec<_>>()
            )
        };
        assert_eq!(shape(&first), shape(&second));
    }
}
