use std::collections::HashSet;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use super::review::review_from_candidates;
use caudra_storage::permission_state::PermissionReviewSource;
use caudra_storage::permission_state::{
    PermissionArgumentConstraint, PermissionResourceKind, PermissionResourceSelector,
    PermissionRuleRecord, PermissionStateError, PermissionSubject, StructuredPermissionEffect,
    StructuredPermissionRule, inventory_fingerprint, read_inventory, replace_reviewed,
};
use caudra_storage::{StateDir, paths::incremental_canonicalize};
use color_eyre::eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const WORKDIR: &str = "workdir";
const SHORT_ID_LENGTH: usize = 8;
const FILESYSTEM_SUBTREE: &str = "filesystem_subtree";
const NEEDS_CANDIDATE: &str =
    "unresolved hash; supply an explicit candidate or issue a fresh grant";
const INPUT_UNSUPPORTED: &str =
    "exact or selected input constraints require a fresh grant; input rewriting is unsupported";
const RESTRICTIVE_BLOCKER: &str =
    "related restrictive policy must be verifiable and selected before transferring allows";
const ANCESTOR_RESTRICTION: &str = "restrictive ancestor subtree would not cover the destination; fresh restrictive authorization is required";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebindCandidates {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DestinationIdentity {
    pub root: PathBuf,
    pub device: u64,
    pub inode: u64,
}

impl DestinationIdentity {
    pub fn inspect(root: &Path) -> Result<Self> {
        validate_absolute(root)?;
        if fs::canonicalize(root)? != root {
            bail!("destination must be its explicit canonical directory, not an alias");
        }
        let directory = open_directory(root)?;
        #[cfg(unix)]
        {
            let metadata = directory.metadata()?;
            Ok(Self {
                root: root.to_owned(),
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = directory;
            bail!("permission rebinding requires a supported physical directory identity");
        }
    }

    fn recheck(&self, directory: &File) -> Result<()> {
        #[cfg(unix)]
        {
            let metadata = directory.metadata()?;
            if metadata.dev() != self.device || metadata.ino() != self.inode {
                bail!("held destination directory does not match the reviewed identity");
            }
        }
        #[cfg(not(unix))]
        let _ = directory;
        if Self::inspect(&self.root)? != *self {
            bail!("destination directory identity changed; review a new preview");
        }
        Ok(())
    }
}

fn open_directory(root: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        Ok(OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(root)?)
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        bail!("no-follow permission directory handles are unsupported on this platform");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RebindClassification {
    Verifiable,
    NeedsCandidate,
    Unaffected,
    Unsupported,
    RestrictivePolicyBlocker,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifiedLabel {
    pub field: String,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Serialize)]
pub struct InventoryRow {
    pub record: PermissionRuleRecord,
    pub binding_status: &'static str,
    pub age_seconds: u64,
    pub short_id: String,
    pub labels: Vec<VerifiedLabel>,
}

pub fn inventory(
    records: &[PermissionRuleRecord],
    project: &Path,
    known_roots: &[String],
    now: u64,
) -> Result<Vec<InventoryRow>> {
    inventory_fingerprint(records)?;
    validate_absolute(project)?;
    let candidates = RebindCandidates {
        paths: known_roots.to_vec(),
        values: Vec::new(),
    };
    validate_candidates(&candidates)?;
    records
        .iter()
        .map(|record| {
            let (_, labels, _) = transform(record, project, project, &candidates);
            Ok(InventoryRow {
                record: record.clone(),
                binding_status: if !record.is_active() {
                    "revoked"
                } else if record
                    .project
                    .as_deref()
                    .is_some_and(|bound| bound != project)
                {
                    "excluded_other_project"
                } else {
                    "eligible_binding_policy_not_evaluated"
                },
                age_seconds: now.saturating_sub(record.created_at),
                short_id: record.id.chars().take(SHORT_ID_LENGTH).collect(),
                labels,
            })
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct RebindRow {
    pub original: PermissionRuleRecord,
    pub classification: RebindClassification,
    pub selected: bool,
    pub retain_original: bool,
    pub replacement_project: Option<PathBuf>,
    pub replacement_rule: Option<StructuredPermissionRule>,
    pub labels: Vec<VerifiedLabel>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RebindPreview {
    pub inventory_fingerprint: String,
    pub old_root: PathBuf,
    pub destination: DestinationIdentity,
    pub rows: Vec<RebindRow>,
    pub partial_policy_transfer: bool,
}

impl RebindPreview {
    pub fn confirmation(&self) -> Result<String> {
        Ok(hash_bytes(&serde_json::to_vec(self)?))
    }

    pub fn can_apply(&self) -> bool {
        self.rows.iter().any(|row| row.selected)
            && self
                .rows
                .iter()
                .filter(|row| row.selected)
                .all(|row| row.classification == RebindClassification::Verifiable)
    }
}

pub fn preview(
    records: &[PermissionRuleRecord],
    old_root: &Path,
    destination: DestinationIdentity,
    candidates: &RebindCandidates,
    selected: &[String],
) -> Result<RebindPreview> {
    validate_absolute(old_root)?;
    validate_absolute(&destination.root)?;
    if old_root == destination.root {
        bail!("old and destination roots must differ");
    }
    validate_candidates(candidates)?;
    let fingerprint = inventory_fingerprint(records)?;
    let selections: HashSet<_> = selected.iter().collect();
    if selections.len() != selected.len()
        || selections
            .iter()
            .any(|id| !records.iter().any(|record| &record.id == *id))
    {
        bail!("selection contains duplicate or unknown full rule IDs");
    }
    let mut candidates = candidates.clone();
    candidates.paths.push(path_string(old_root)?.to_owned());
    candidates
        .paths
        .push(path_string(&destination.root)?.to_owned());
    let mut rows: Vec<_> = records
        .iter()
        .map(|record| {
            let (rule, labels, mut reasons) =
                transform(record, old_root, &destination.root, &candidates);
            let lost_restriction = loses_ancestor_restriction(record, old_root, &destination.root);
            if lost_restriction {
                reasons.push(ANCESTOR_RESTRICTION.into());
            }
            let project_changed = record.project.as_deref() == Some(old_root);
            let resource_changed = labels.iter().any(|label| label.old != label.new);
            let potentially_affected =
                project_changed || resource_changed || record.project.is_none();
            let unsupported = matches!(
                record.rule.subject,
                PermissionSubject::RemoteWorkcell { .. }
                    | PermissionSubject::RemoteNative { .. }
                    | PermissionSubject::UnknownLegacy { .. }
            );
            let classification = if !record.is_active() || !potentially_affected {
                RebindClassification::Unaffected
            } else if lost_restriction {
                RebindClassification::RestrictivePolicyBlocker
            } else if unsupported {
                RebindClassification::Unsupported
            } else if !reasons.is_empty() {
                RebindClassification::NeedsCandidate
            } else if project_changed || resource_changed {
                RebindClassification::Verifiable
            } else {
                RebindClassification::Unaffected
            };
            let replacement_rule =
                (classification == RebindClassification::Verifiable).then_some(rule);
            RebindRow {
                original: record.clone(),
                classification,
                selected: selections.contains(&record.id),
                retain_original: record.rule.effect != StructuredPermissionEffect::Allow,
                replacement_project: if project_changed {
                    Some(destination.root.clone())
                } else {
                    record.project.clone()
                },
                replacement_rule,
                labels,
                reasons,
            }
        })
        .collect();
    let restrictive_blocker = rows.iter().any(|row| {
        row.original.is_active()
            && row.original.rule.effect != StructuredPermissionEffect::Allow
            && (row.original.project.is_none()
                || row.original.project.as_deref() == Some(old_root)
                || row.labels.iter().any(|label| label.old != label.new))
            && row.classification != RebindClassification::Unaffected
            && !(row.selected && row.classification == RebindClassification::Verifiable)
    });
    if restrictive_blocker {
        for row in &mut rows {
            if row.classification == RebindClassification::Verifiable
                && row.original.rule.effect == StructuredPermissionEffect::Allow
            {
                row.classification = RebindClassification::RestrictivePolicyBlocker;
                row.replacement_rule = None;
                row.reasons.push(RESTRICTIVE_BLOCKER.into());
            }
        }
    }
    let partial_policy_transfer = rows.iter().any(|row| {
        row.original.is_active()
            && row.classification != RebindClassification::Unaffected
            && !row.selected
    }) || rows.iter().any(|row| {
        matches!(
            row.classification,
            RebindClassification::NeedsCandidate
                | RebindClassification::Unsupported
                | RebindClassification::RestrictivePolicyBlocker
        )
    });
    Ok(RebindPreview {
        inventory_fingerprint: fingerprint,
        old_root: old_root.to_owned(),
        destination,
        rows,
        partial_policy_transfer,
    })
}

fn loses_ancestor_restriction(record: &PermissionRuleRecord, old: &Path, new: &Path) -> bool {
    record.rule.effect != StructuredPermissionEffect::Allow
        && (record.project.is_none() || record.project.as_deref() == Some(old))
        && record.rule.resources.iter().any(|resource| {
            [&resource.selector]
                .into_iter()
                .chain(resource.attributes.values())
                .any(|selector| {
                    let PermissionResourceSelector::FilesystemSubtreeDigest { digest } = selector
                    else {
                        return false;
                    };
                    old.ancestors().any(|ancestor| {
                        ancestor != old
                            && !new.starts_with(ancestor)
                            && ancestor
                                .to_str()
                                .is_some_and(|path| value_digest(path, true) == *digest)
                    })
                })
        })
}

pub fn apply(
    state_dir: &StateDir,
    old_root: &Path,
    destination: DestinationIdentity,
    candidates: &RebindCandidates,
    selected: &[String],
    confirmation: &str,
) -> Result<Vec<PermissionRuleRecord>> {
    let records = read_inventory(state_dir)?;
    let preview = preview(&records, old_root, destination, candidates, selected)?;
    if preview.confirmation()? != confirmation {
        bail!("preview fingerprint changed; review and confirm a fresh preview");
    }
    if !preview.can_apply() {
        bail!(
            "selected rules are not all verifiable; resolve candidates and restrictive policy first"
        );
    }
    let directory = open_directory(&preview.destination.root)?;
    preview.destination.recheck(&directory)?;
    let replacements = preview
        .rows
        .iter()
        .filter(|row| row.selected)
        .map(|row| {
            let rule = row
                .replacement_rule
                .clone()
                .ok_or_else(|| eyre!("selected replacement is missing"))?;
            let candidates = row
                .labels
                .iter()
                .map(|label| label.new.clone())
                .collect::<Vec<_>>();
            let tool = row
                .original
                .review
                .as_ref()
                .map(|review| review.tool.as_str())
                .unwrap_or_else(|| match &rule.subject {
                    PermissionSubject::Native { contract, .. }
                    | PermissionSubject::RemoteNative { contract, .. } => contract,
                    PermissionSubject::Mcp { tool, .. } => tool,
                    _ => "Bound tool",
                });
            let review = review_from_candidates(
                &rule,
                tool,
                None,
                &candidates,
                PermissionReviewSource::Recovered,
            );
            Ok((
                row.original.id.clone(),
                row.replacement_project.clone(),
                rule,
                Some(review),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(replace_reviewed(
        state_dir,
        &preview.inventory_fingerprint,
        replacements,
        || {
            let check = || -> Result<()> {
                preview.destination.recheck(&directory)?;
                for label in preview
                    .rows
                    .iter()
                    .filter(|row| row.selected)
                    .flat_map(|row| &row.labels)
                {
                    if label.old != label.new {
                        let path = Path::new(&label.new);
                        if incremental_canonicalize(path).as_deref() != Some(path) {
                            bail!(
                                "destination resource resolves through an alias; issue a fresh grant"
                            );
                        }
                    }
                }
                Ok(())
            };
            check().map_err(|error| PermissionStateError::Invalid(error.to_string()))
        },
    )?)
}

fn transform(
    record: &PermissionRuleRecord,
    old: &Path,
    new: &Path,
    candidates: &RebindCandidates,
) -> (StructuredPermissionRule, Vec<VerifiedLabel>, Vec<String>) {
    let mut rule = record.rule.clone();
    let mut labels = Vec::new();
    let mut reasons = Vec::new();
    if !matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained) {
        reasons.push(INPUT_UNSUPPORTED.into());
    }
    for (index, resource) in rule.resources.iter_mut().enumerate() {
        let filesystem = matches!(
            resource.kind,
            PermissionResourceKind::File | PermissionResourceKind::Directory
        );
        transform_selector(
            &mut resource.selector,
            filesystem,
            &format!("resources/{index}/selector"),
            old,
            new,
            candidates,
            &mut labels,
            &mut reasons,
        );
        for (attribute, selector) in &mut resource.attributes {
            transform_selector(
                selector,
                attribute == WORKDIR,
                &format!("resources/{index}/attributes/{attribute}"),
                old,
                new,
                candidates,
                &mut labels,
                &mut reasons,
            );
        }
    }
    (rule, labels, reasons)
}

#[allow(clippy::too_many_arguments)]
fn transform_selector(
    selector: &mut PermissionResourceSelector,
    filesystem: bool,
    field: &str,
    old: &Path,
    new: &Path,
    candidates: &RebindCandidates,
    labels: &mut Vec<VerifiedLabel>,
    reasons: &mut Vec<String>,
) {
    let (digest, subtree) = match selector {
        PermissionResourceSelector::Digest { digest } => (digest, false),
        PermissionResourceSelector::FilesystemSubtreeDigest { digest } if filesystem => {
            (digest, true)
        }
        PermissionResourceSelector::Any => return,
        PermissionResourceSelector::CommandPattern { pattern } => {
            if old.to_str().is_some_and(|old| pattern.contains(old)) {
                reasons.push(format!(
                    "{field}: command pattern mentions the old root; issue a fresh grant"
                ));
            }
            return;
        }
        _ => {
            reasons.push(format!("{field}: unsupported selector"));
            return;
        }
    };
    let values = if filesystem {
        &candidates.paths
    } else {
        &candidates.values
    };
    let Some(candidate) = values
        .iter()
        .find(|value| value_digest(value, subtree) == *digest)
    else {
        reasons.push(format!("{field}: {NEEDS_CANDIDATE}"));
        return;
    };
    let replacement = if filesystem {
        Path::new(candidate)
            .strip_prefix(old)
            .ok()
            .map(|relative| {
                if relative.as_os_str().is_empty() {
                    new.to_owned()
                } else {
                    new.join(relative)
                }
            })
            .and_then(|path| path.to_str().map(str::to_owned))
            .unwrap_or_else(|| candidate.clone())
    } else {
        if old.to_str().is_some_and(|old| candidate.contains(old)) {
            reasons.push(format!(
                "{field}: value mentions old root; textual rewriting is unsupported"
            ));
        }
        candidate.clone()
    };
    *digest = value_digest(&replacement, subtree);
    labels.push(VerifiedLabel {
        field: field.into(),
        old: candidate.clone(),
        new: replacement,
    });
}

fn value_digest(value: &str, subtree: bool) -> String {
    let value = if subtree {
        json!([FILESYSTEM_SUBTREE, value])
    } else {
        Value::String(value.into())
    };
    hash_bytes(value.to_string().as_bytes())
}

fn hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_candidates(candidates: &RebindCandidates) -> Result<()> {
    for path in &candidates.paths {
        validate_absolute(Path::new(path))?;
    }
    Ok(())
}

fn path_string(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre!("permission paths must be UTF-8"))
}

fn validate_absolute(path: &Path) -> Result<()> {
    let text = path_string(path)?;
    if !path.is_absolute()
        || text.contains('\0')
        || path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        || path.components().collect::<PathBuf>().as_os_str() != path.as_os_str()
    {
        bail!(
            "permission paths must be normalized absolute strings without aliases or dot segments"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::slice::from_ref;

    use caudra_storage::id::CaudraId;
    use caudra_storage::permission_state::{
        PermissionExecutorKind, PermissionLifetime, PermissionResourceAccess,
        PermissionResourceConstraint,
    };
    #[cfg(unix)]
    use caudra_storage::permission_state::{
        PermissionState, PermissionStateError, inventory_fingerprint, replace_reviewed,
    };
    use test_case::test_case;

    use super::{
        ANCESTOR_RESTRICTION, DestinationIdentity, INPUT_UNSUPPORTED, Path,
        PermissionArgumentConstraint, PermissionResourceKind, PermissionResourceSelector,
        PermissionRuleRecord, PermissionSubject, RESTRICTIVE_BLOCKER, RebindCandidates,
        RebindClassification, StructuredPermissionEffect, StructuredPermissionRule, WORKDIR,
        inventory, preview, validate_absolute, value_digest,
    };
    #[cfg(unix)]
    use super::{StateDir, apply, fs, open_directory, read_inventory};

    const OLD: &str = "/historical/project";
    const NEW: &str = "/destination/project";
    const SIBLING: &str = "/workspace/sibling";

    #[test_case(true, StructuredPermissionEffect::Deny; "project_deny")]
    #[test_case(false, StructuredPermissionEffect::Deny; "global_deny")]
    #[test_case(true, StructuredPermissionEffect::Ask; "project_ask")]
    #[test_case(false, StructuredPermissionEffect::Ask; "global_ask")]
    fn ancestor_restrictions_cannot_be_left_outside_destination(
        project: bool,
        effect: StructuredPermissionEffect,
    ) {
        const SOURCE: &str = "/workspace/maki";
        const DESTINATION: &str = "/elsewhere/caudra";
        const ANCESTOR: &str = "/workspace";
        let mut allow = record(true);
        allow.project = Some(SOURCE.into());
        allow.rule.resources[0].kind = PermissionResourceKind::Directory;
        allow.rule.resources[0].selector = PermissionResourceSelector::FilesystemSubtreeDigest {
            digest: value_digest(SOURCE, true),
        };
        allow.rule.resources[0].attributes.clear();
        let mut deny = allow.clone();
        deny.id = CaudraId::generate().to_string();
        deny.project = project.then(|| SOURCE.into());
        deny.rule.lifetime = if project {
            PermissionLifetime::Project
        } else {
            PermissionLifetime::Global
        };
        deny.rule.effect = effect;
        deny.rule.resources[0].selector = PermissionResourceSelector::FilesystemSubtreeDigest {
            digest: value_digest(ANCESTOR, true),
        };
        for selected in [
            vec![allow.id.clone()],
            vec![allow.id.clone(), deny.id.clone()],
        ] {
            let result = preview(
                &[allow.clone(), deny.clone()],
                Path::new(SOURCE),
                DestinationIdentity {
                    root: DESTINATION.into(),
                    device: 1,
                    inode: 1,
                },
                &RebindCandidates {
                    paths: vec![ANCESTOR.into()],
                    values: Vec::new(),
                },
                &selected,
            )
            .unwrap();
            assert!(!result.can_apply());
            assert_eq!(
                result.rows[0].classification,
                RebindClassification::RestrictivePolicyBlocker
            );
            assert_eq!(
                result.rows[1].classification,
                RebindClassification::RestrictivePolicyBlocker
            );
            assert!(
                result.rows[1]
                    .reasons
                    .iter()
                    .any(|reason| reason == ANCESTOR_RESTRICTION)
            );
        }
    }

    #[cfg(unix)]
    #[test_case(true, StructuredPermissionEffect::Deny; "project_deny")]
    #[test_case(false, StructuredPermissionEffect::Deny; "global_deny")]
    #[test_case(true, StructuredPermissionEffect::Ask; "project_ask")]
    #[test_case(false, StructuredPermissionEffect::Ask; "global_ask")]
    fn old_root_subtree_restriction_moves_and_retains_original(
        project: bool,
        effect: StructuredPermissionEffect,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let new = temp.path().join("destination");
        fs::create_dir(&new).unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&dir).unwrap();
        let mut rule = record(project).rule;
        rule.resources[0].kind = PermissionResourceKind::Directory;
        rule.resources[0].access = Some(PermissionResourceAccess::Read);
        rule.resources[0].selector = PermissionResourceSelector::FilesystemSubtreeDigest {
            digest: value_digest(OLD, true),
        };
        rule.resources[0].attributes.clear();
        let allow = state
            .insert(project.then(|| OLD.into()), rule.clone())
            .unwrap();
        rule.effect = effect;
        let restriction = state.insert(project.then(|| OLD.into()), rule).unwrap();
        let selected = vec![allow.id.clone(), restriction.id.clone()];
        let destination = DestinationIdentity::inspect(&new).unwrap();
        let candidates = RebindCandidates::default();
        let preview = preview(
            state.records(),
            Path::new(OLD),
            destination.clone(),
            &candidates,
            &selected,
        )
        .unwrap();
        assert!(preview.can_apply());
        assert!(preview.rows[1].retain_original);
        let confirmation = preview.confirmation().unwrap();
        drop(state);
        let inserted = apply(
            &dir,
            Path::new(OLD),
            destination,
            &candidates,
            &selected,
            &confirmation,
        )
        .unwrap();
        assert_eq!(inserted[1].rule.effect, restriction.rule.effect);
        assert_eq!(inserted[1].project, project.then(|| new.clone()));
        assert_eq!(
            inserted[1].rule.resources[0].selector,
            PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: value_digest(new.to_str().unwrap(), true),
            }
        );
        let records = read_inventory(&dir).unwrap();
        assert_eq!(
            records.iter().find(|record| record.id == restriction.id),
            Some(&restriction)
        );
        assert!(
            records
                .iter()
                .any(|record| record.id == allow.id && !record.is_active())
        );
    }

    #[cfg(unix)]
    #[test]
    fn destination_handle_rejects_alias_and_detects_swap_before_commit() {
        let temp = tempfile::tempdir().unwrap();
        let new = temp.path().join("new");
        fs::create_dir(&new).unwrap();
        let alias = temp.path().join("alias");
        symlink(&new, &alias).unwrap();
        assert!(open_directory(&alias).is_err());
        let destination = DestinationIdentity::inspect(&new).unwrap();
        let handle = open_directory(&new).unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&dir).unwrap();
        let source = state.insert(Some(OLD.into()), record(true).rule).unwrap();
        let original = state.records().to_vec();
        let fingerprint = inventory_fingerprint(&original).unwrap();
        drop(state);
        let result = replace_reviewed(
            &dir,
            &fingerprint,
            vec![(source.id, Some(new.clone()), source.rule, None)],
            || {
                fs::rename(&new, temp.path().join("retired")).unwrap();
                fs::create_dir(&new).unwrap();
                destination
                    .recheck(&handle)
                    .map_err(|error| PermissionStateError::Invalid(error.to_string()))
            },
        );
        assert!(result.is_err());
        assert_eq!(read_inventory(&dir).unwrap(), original);
    }

    fn destination() -> DestinationIdentity {
        DestinationIdentity {
            root: NEW.into(),
            device: 1,
            inode: 1,
        }
    }

    fn record(project: bool) -> PermissionRuleRecord {
        PermissionRuleRecord {
            id: CaudraId::generate().to_string(),
            project: project.then(|| OLD.into()),
            rule: StructuredPermissionRule {
                subject: PermissionSubject::Native {
                    owner: "workcell".into(),
                    contract: "shell".into(),
                },
                executor: PermissionExecutorKind::Native,
                resources: vec![PermissionResourceConstraint {
                    kind: PermissionResourceKind::Command,
                    selector: PermissionResourceSelector::CommandPattern {
                        pattern: "cargo test *".into(),
                    },
                    access: Some(PermissionResourceAccess::Execute),
                    protected: Some(false),
                    attributes: BTreeMap::from([(
                        WORKDIR.into(),
                        PermissionResourceSelector::Digest {
                            digest: value_digest(OLD, false),
                        },
                    )]),
                }],
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: if project {
                    PermissionLifetime::Project
                } else {
                    PermissionLifetime::Global
                },
                effect: StructuredPermissionEffect::Allow,
                family: None,
            },
            review: None,
            label: None,
            replaces: None,
            created_at: 1,
            revoked_at: None,
        }
    }

    #[test_case(true; "project")]
    #[test_case(false; "global")]
    fn preview_rebinds_workdir_without_changing_constraints(project: bool) {
        let source = record(project);
        let result = preview(
            from_ref(&source),
            Path::new(OLD),
            destination(),
            &RebindCandidates::default(),
            from_ref(&source.id),
        )
        .unwrap();
        assert!(result.can_apply());
        let row = &result.rows[0];
        let mut expected = source.rule;
        expected.resources[0].attributes.insert(
            WORKDIR.into(),
            PermissionResourceSelector::Digest {
                digest: value_digest(NEW, false),
            },
        );
        assert_eq!(row.replacement_rule.as_ref(), Some(&expected));
        assert_eq!(row.replacement_project, project.then(|| NEW.into()));
    }

    #[test_case(false; "unknown_descendant")]
    #[test_case(true; "verified_descendant_and_sibling")]
    fn descendants_need_explicit_candidates(known: bool) {
        let mut source = record(true);
        let descendant = format!("{OLD}/src");
        let resource = &mut source.rule.resources[0];
        resource.kind = PermissionResourceKind::Directory;
        resource.selector = PermissionResourceSelector::FilesystemSubtreeDigest {
            digest: value_digest(&descendant, true),
        };
        resource.attributes.insert(
            "sibling".into(),
            PermissionResourceSelector::Digest {
                digest: value_digest(SIBLING, false),
            },
        );
        let candidates = if known {
            RebindCandidates {
                paths: vec![descendant],
                values: vec![SIBLING.into()],
            }
        } else {
            RebindCandidates::default()
        };
        let result = preview(
            &[source.clone()],
            Path::new(OLD),
            destination(),
            &candidates,
            &[source.id.clone()],
        )
        .unwrap();
        assert_eq!(result.can_apply(), known);
        if known {
            let replacement = result.rows[0].replacement_rule.as_ref().unwrap();
            assert_eq!(
                replacement.resources[0].selector,
                PermissionResourceSelector::FilesystemSubtreeDigest {
                    digest: value_digest(&format!("{NEW}/src"), true)
                }
            );
            assert_eq!(
                replacement.resources[0].attributes["sibling"],
                source.rule.resources[0].attributes["sibling"]
            );
        } else {
            assert_eq!(
                result.rows[0].classification,
                RebindClassification::NeedsCandidate
            );
        }
    }

    #[test_case(false; "deny_not_selected")]
    #[test_case(true; "deny_selected")]
    fn restrictive_policy_must_move_with_allows(selected_deny: bool) {
        let allow = record(true);
        let mut deny = record(true);
        deny.rule.effect = StructuredPermissionEffect::Deny;
        let mut selected = vec![allow.id.clone()];
        if selected_deny {
            selected.push(deny.id.clone());
        }
        let result = preview(
            &[allow, deny],
            Path::new(OLD),
            destination(),
            &RebindCandidates::default(),
            &selected,
        )
        .unwrap();
        assert_eq!(result.can_apply(), selected_deny);
        if selected_deny {
            assert_eq!(
                result.rows[1].replacement_rule.as_ref().unwrap().effect,
                StructuredPermissionEffect::Deny
            );
        } else {
            assert_eq!(result.rows[0].reasons, [RESTRICTIVE_BLOCKER]);
        }
    }

    #[test_case(true; "project")]
    #[test_case(false; "global")]
    fn unknown_deny_blocks_even_when_selected(project: bool) {
        let allow = record(true);
        let mut deny = record(project);
        deny.rule.effect = StructuredPermissionEffect::Deny;
        deny.rule.arguments = PermissionArgumentConstraint::Exact {
            digest: value_digest("unknown", false),
        };
        let selected = vec![allow.id.clone(), deny.id.clone()];
        let result = preview(
            &[allow, deny],
            Path::new(OLD),
            destination(),
            &RebindCandidates::default(),
            &selected,
        )
        .unwrap();
        assert!(!result.can_apply());
        assert_eq!(
            result.rows[0].classification,
            RebindClassification::RestrictivePolicyBlocker
        );
        assert_eq!(result.rows[1].reasons, [INPUT_UNSUPPORTED]);
    }

    #[test]
    fn confirmation_binds_selection_and_inventory() {
        let source = record(true);
        let records = vec![source.clone()];
        let candidates = RebindCandidates::default();
        let unselected =
            preview(&records, Path::new(OLD), destination(), &candidates, &[]).unwrap();
        let selected = preview(
            &records,
            Path::new(OLD),
            destination(),
            &candidates,
            from_ref(&source.id),
        )
        .unwrap();
        assert_ne!(
            unselected.confirmation().unwrap(),
            selected.confirmation().unwrap()
        );
        let mut changed = records;
        changed[0].revoked_at = Some(2);
        let stale = preview(
            &changed,
            Path::new(OLD),
            destination(),
            &candidates,
            &[source.id],
        )
        .unwrap();
        assert_ne!(
            selected.confirmation().unwrap(),
            stale.confirmation().unwrap()
        );
        assert!(!stale.can_apply());
    }

    #[test]
    fn inventory_includes_excluded_and_revoked_records() {
        let active = record(true);
        let mut revoked = record(false);
        revoked.revoked_at = Some(2);
        let rows = inventory(&[active, revoked], Path::new(NEW), &[OLD.into()], 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].binding_status, "excluded_other_project");
        assert_eq!(rows[0].labels[0].old, OLD);
        assert_eq!(rows[1].binding_status, "revoked");
    }

    #[test_case("relative"; "relative")]
    #[test_case("/historical/../project"; "parent")]
    #[test_case("/historical/./project"; "dot")]
    #[test_case("/historical//project"; "double_separator")]
    #[test_case("/historical/project/"; "trailing_separator")]
    fn historical_paths_are_validated_without_normalizing(path: &str) {
        assert!(validate_absolute(Path::new(path)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn historical_symlink_is_not_followed_by_preview() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("old");
        let new = temp.path().join("new");
        fs::create_dir(&new).unwrap();
        symlink(&new, &old).unwrap();
        let mut source = record(true);
        source.project = Some(old.clone());
        source.rule.resources[0].attributes.insert(
            WORKDIR.into(),
            PermissionResourceSelector::Digest {
                digest: value_digest(old.to_str().unwrap(), false),
            },
        );
        let result = preview(
            &[source.clone()],
            &old,
            DestinationIdentity::inspect(&new).unwrap(),
            &RebindCandidates::default(),
            &[source.id],
        )
        .unwrap();
        assert!(result.can_apply());
        assert_eq!(result.rows[0].labels[0].old, old.to_str().unwrap());
        assert_eq!(result.rows[0].labels[0].new, new.to_str().unwrap());
    }

    #[cfg(unix)]
    #[test_case(false; "apply")]
    #[test_case(true; "identity_changed")]
    fn apply_rechecks_identity_and_rebuilds_review_provenance(change_identity: bool) {
        let temp = tempfile::tempdir().unwrap();
        let new = temp.path().join("new");
        fs::create_dir(&new).unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&dir).unwrap();
        let rule = record(true).rule;
        let source = state.insert(Some(OLD.into()), rule).unwrap();
        let original = state.records().to_vec();
        drop(state);
        let destination = DestinationIdentity::inspect(&new).unwrap();
        let selected = vec![source.id.clone()];
        let candidates = RebindCandidates::default();
        let result = preview(
            &original,
            Path::new(OLD),
            destination.clone(),
            &candidates,
            &selected,
        )
        .unwrap();
        let confirmation = result.confirmation().unwrap();
        if change_identity {
            fs::rename(&new, temp.path().join("retired")).unwrap();
            fs::create_dir(&new).unwrap();
        }
        let applied = apply(
            &dir,
            Path::new(OLD),
            destination,
            &candidates,
            &selected,
            &confirmation,
        );
        if change_identity {
            assert!(applied.is_err());
            assert_eq!(read_inventory(&dir).unwrap(), original);
        } else {
            let inserted = applied.unwrap();
            assert_eq!(inserted[0].project.as_deref(), Some(new.as_path()));
            let review = inserted[0].review.as_ref().unwrap();
            assert_eq!(review.source, super::PermissionReviewSource::Recovered);
            assert!(review.resources[0].attributes[WORKDIR].contains(new.to_str().unwrap()));
            assert!(!serde_json::to_string(review).unwrap().contains(OLD));
            assert_eq!(
                read_inventory(&dir)
                    .unwrap()
                    .iter()
                    .filter(|row| row.is_active())
                    .count(),
                1
            );
        }
    }
}
