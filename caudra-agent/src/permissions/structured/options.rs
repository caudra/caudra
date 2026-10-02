use super::POSSIBLE_WORKDIRS_ATTRIBUTE;
use super::canonical_json_sha256;
use super::{
    BROWSE_DIRECT, BROWSE_RECURSION_ATTRIBUTE, BROWSE_RECURSIVE, GIT_METADATA_DIR,
    PermissionArgumentConstraint, PermissionAuthorityProfile, PermissionCapabilityFamily,
    PermissionCaution, PermissionExecutorKind, PermissionLifetime, PermissionOptionGroup,
    PermissionResource, PermissionResourceAccess, PermissionResourceConstraint,
    PermissionResourceKind, PermissionResourceSelector, PermissionRuleOption, PermissionSubject,
    StructuredPermissionEffect, StructuredPermissionRule, WORKDIR_ATTRIBUTE,
    exact_resource_constraints, filesystem_subtree_digest, is_filesystem_browse_subject,
    is_filesystem_read_access, is_filesystem_read_kind, is_filesystem_read_subject,
    listed_commands, normalized_filesystem_path, pinned_digest, remote_resource_identity,
    resource_constraint, resource_value_digest, reusable_remote_resource_constraint, safe_summary,
    selected_input_digest, strict_http_url, url_origin_digest, url_subtree_digest,
    url_subtree_roots,
};
use super::{
    PermissionRequest, ResourceCoverage,
    review::{COMMAND_TEMPLATE_EXECUTION_NOTICE, command_template_phrase},
    trusted_command_observation,
};
use crate::permissions::{
    command_pattern::{ancestor_prefixes, grade_command_pattern, reusable_prefix},
    manager::PatternCandidates,
    pattern_matching::CompiledPattern,
    pattern_recognition::{
        CandidateEvidence, CommandObservation, InvocationOutcome, ObservationProvenance,
    },
};
use caudra_config::ToolKey;
use caudra_storage::permission_patterns::{
    MAX_PATTERN_JSON_BYTES, PatternDefinition, PatternToken,
};
use caudra_storage::permission_state::validate_command_templates;
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(super) const SUBTREE_OPTION_ID: &str = "allow_filesystem_subtree";

pub(super) const URL_SUBTREE_OPTION_ID: &str = "allow_url_subtree";

pub(super) const URL_ORIGIN_OPTION_ID: &str = "allow_url_origin";

pub(super) const EXACT_CALL_OPTION: &str = "allow_exact";

/// How many path prefixes a URL ladder offers below its origin. The rungs share
/// one row, so this bounds the request a deep path serializes to rather than
/// anything the prompt draws. Real paths stay well under it.
pub(super) const MAX_URL_LADDER_RUNGS: usize = 8;

pub(super) const OUTSIDE_HOME_PHRASE: &str = "ALLOW OUTSIDE HOME";

pub(in crate::permissions) const BROAD_SHELL_PHRASE: &str = "ALLOW BROAD SHELL ACCESS";

pub const COMMAND_GROUP_PREFIX: &str = "command_";

pub const COMMAND_EXACT_PREFIX: &str = "command_exact_";

pub const COMMAND_PATTERN_PREFIX: &str = "command_pattern_";
pub const COMMAND_TEMPLATE_PREFIX: &str = "command_template_";
const COMMAND_PREFIX_PREFIX: &str = "command_prefix_";

pub(super) const EXACT_COMMAND_CHIP: &str = "this command";

pub(super) const EXACT_COMMANDS_OPTION: &str = "allow_exact_commands";

pub(super) const COMMAND_PATTERNS_OPTION: &str = "allow_command_patterns";

/// The whole-request shell authorities a per-command answer reproduces exactly:
/// every row on its narrowest reusable rung is one, every row at its widest is
/// the other. A prompt that offers the rows has no reason to offer these too.
pub const COMPOSABLE_SHELL_OPTIONS: &[&str] = &[EXACT_COMMANDS_OPTION, COMMAND_PATTERNS_OPTION];
const MAX_COMMAND_TEMPLATE_OPTIONS: usize = 16;
const MAX_COMMAND_TEMPLATE_OPTION_BYTES: usize = 4 * MAX_PATTERN_JSON_BYTES;
const BROWSE_OPTION_ID: &str = "allow_filesystem_browse";
const BROWSE_SUBTREE_OPTION_ID: &str = "allow_filesystem_browse_subtree";
const BROWSE_NAMES_LABEL: &str = "this folder and below";
const EXACT_CALL_LABEL: &str = "this call";
const EXACT_COMMAND_LABEL: &str = "this exact command";
const EXACT_COMMANDS_LABEL: &str = "these exact commands";
const COMMAND_PATTERNS_LABEL: &str = "these command patterns";
const ANY_COMMAND_LABEL: &str = "any shell command";
const PROJECT_COMMANDS_LABEL: &str = "any command in this folder";
pub(super) const WORKDIRS_COMMANDS_LABEL: &str = "any command in these folders";
const REMOTE_RESOURCES_LABEL: &str = "these remote resources";
const EXACT_PAGE_LABEL: &str = "this exact page";
const PAGE_SUBTREE_LABEL: &str = "this page and below";
const ANY_PAGE_LABEL: &str = "any public web page";
const EXACT_QUERY_LABEL: &str = "this exact search";
const ANY_QUERY_LABEL: &str = "any search";
const ANY_ARGUMENTS_LABEL: &str = "any arguments";
const EXACT_SEARCH_LABEL: &str = "this search";
const EXACT_FILE_LABEL: &str = "this file";
const EXACT_FOLDER_LABEL: &str = "this folder";
const EXACT_FILES_LABEL: &str = "these files";
const EXACT_PATHS_LABEL: &str = "these paths";
const SUBTREES_LABEL: &str = "these folders";
const PROJECT_LABEL: &str = "this project";
const FILESYSTEM_ROOT_LABEL: &str = "anything under /";
const HOME_PREFIX: &str = "~/";
const HTTPS_SCHEME: &str = "https://";
const IMPORTED_PATTERN_CONTEXT: &str = "Historical execution context and tool identity are unverified. Analysis assumes standard Bash startup, no aliases, functions, traps or command-not-found hook, default shell options and standard builtins, standard directory variables, empty CDPATH, disabled lastpipe, and logical PWD matching the initial cwd. The session's current stored cwd approximates its historical project; paths are interpreted lexically without filesystem resolution. These assumptions do not verify the historical environment.";

impl CandidateEvidence {
    pub fn review_origin(&self) -> &'static str {
        match self.provenance {
            ObservationProvenance::Native => "Native observations",
            ObservationProvenance::Imported => "Imported history (unverified)",
            ObservationProvenance::Legacy => "Legacy history (unverified)",
            ObservationProvenance::Unknown => "Unknown provenance (unverified)",
        }
    }

    pub fn review_summary(&self) -> String {
        let outcomes = if self.outcomes.is_empty() {
            "not recorded".into()
        } else {
            self.outcomes
                .iter()
                .map(|(outcome, count)| {
                    let label = match outcome {
                        InvocationOutcome::Requested => {
                            "requested (execution outcome not recorded)"
                        }
                        InvocationOutcome::Succeeded => "succeeded",
                        InvocationOutcome::Failed => "failed",
                        InvocationOutcome::Rejected => "rejected",
                        InvocationOutcome::Unknown => "unknown outcomes",
                    };
                    format!("{count} {label}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut summary = format!(
            "{}: {} observations across {} sessions. Outcomes: {outcomes}.",
            self.review_origin(),
            self.support.observations,
            self.support.independent_sessions,
        );
        if self.provenance == ObservationProvenance::Imported {
            summary.push(' ');
            summary.push_str(IMPORTED_PATTERN_CONTEXT);
        }
        summary
    }
}

impl PermissionRequest {
    pub(in crate::permissions) fn add_pattern_candidates(
        &mut self,
        candidates: &PatternCandidates,
        coverage: &[Option<ResourceCoverage>],
    ) {
        self.options
            .retain(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX));
        let mut count = 0;
        let mut option_bytes = 0;
        'rows: for (index, resource) in self.resources.iter().enumerate() {
            if coverage.get(index).is_some_and(Option::is_some) || resource.requires_prompt {
                continue;
            }
            let Some(observation) = trusted_command_observation(self, resource) else {
                continue;
            };
            let Some(exact) = self
                .options
                .iter()
                .find(|option| option.id == format!("{COMMAND_EXACT_PREFIX}{index}"))
                .cloned()
            else {
                continue;
            };
            let mut seen = BTreeSet::new();
            for candidate in &candidates.proposals {
                let definition = &candidate.definition;
                if candidates.is_dismissed(definition) {
                    continue;
                }
                let Some(mut option) = command_template_option(
                    index,
                    &exact,
                    &observation,
                    definition,
                    &candidate.evidence.review_summary(),
                ) else {
                    continue;
                };
                option.seen = Some(candidate.evidence.support.observations);
                if !seen.insert(option.id.clone()) {
                    continue;
                }
                let Ok(encoded) = serde_json::to_vec(&option.rule) else {
                    continue;
                };
                if count == MAX_COMMAND_TEMPLATE_OPTIONS
                    || option_bytes + encoded.len() > MAX_COMMAND_TEMPLATE_OPTION_BYTES
                {
                    break 'rows;
                }
                option_bytes += encoded.len();
                count += 1;
                let position = rung_position(&self.options, index, &option);
                self.options.insert(position, option);
            }
        }
        prune_incomparable_prefixes(&mut self.options);
        settle_row_defaults(&mut self.options);
    }
}

fn option_row(option: &PermissionRuleOption) -> Option<usize> {
    option.group.as_ref().and_then(|group| group.resource)
}

/// Where a template joins its command's ladder so the ladder keeps widening:
/// after the rungs that fix more literal words, and before a pattern that
/// fixes as many.
fn rung_position(
    options: &[PermissionRuleOption],
    row: usize,
    template: &PermissionRuleOption,
) -> usize {
    let head = literal_head(template);
    let in_row = |option: &PermissionRuleOption| option_row(option) == Some(row);
    options
        .iter()
        .position(|option| {
            in_row(option)
                && !option.id.starts_with(COMMAND_EXACT_PREFIX)
                && (literal_head(option) < head
                    || literal_head(option) == head
                        && !option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
        })
        .or_else(|| options.iter().rposition(in_row).map(|last| last + 1))
        .unwrap_or(options.len())
}

/// A prefix fixing words a template on its row leaves open reaches commands
/// the template does not and misses some it does, so neither widens the
/// other. Only the prefixes of every such template's fixed words stay, which
/// keeps each step along a row wider than the last.
fn prune_incomparable_prefixes(options: &mut Vec<PermissionRuleOption>) {
    let incomparable: Vec<bool> = options
        .iter()
        .map(|option| {
            let Some(words) =
                fixed_words(option).filter(|_| option.id.starts_with(COMMAND_PREFIX_PREFIX))
            else {
                return false;
            };
            options
                .iter()
                .filter(|template| {
                    template.id.starts_with(COMMAND_TEMPLATE_PREFIX)
                        && option_row(template) == option_row(option)
                })
                .filter_map(fixed_words)
                .any(|fixed| !fixed.starts_with(&words))
        })
        .collect();
    let mut incomparable = incomparable.into_iter();
    options.retain(|_| !incomparable.next().unwrap_or_default());
}

fn literal_head(option: &PermissionRuleOption) -> Option<usize> {
    fixed_words(option).map(|words| words.len())
}

/// The literal words a command rung fixes before it matches anything, so the
/// fewer it fixes the wider it reaches.
fn fixed_words(option: &PermissionRuleOption) -> Option<Vec<&str>> {
    match &option.rule.resources.first()?.selector {
        PermissionResourceSelector::CommandPattern { pattern } => Some(
            pattern
                .strip_suffix(" *")
                .unwrap_or(pattern)
                .split_whitespace()
                .collect(),
        ),
        PermissionResourceSelector::CommandTemplate { definition } => Some(
            definition
                .argv
                .iter()
                .map_while(|token| match token {
                    PatternToken::Exact { value, .. } => Some(value.as_str()),
                    PatternToken::Slot { .. } => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Each command's ladder starts on its suggested template, else its reusable
/// prefix, else the command itself.
fn settle_row_defaults(options: &mut [PermissionRuleOption]) {
    let rank = |option: &PermissionRuleOption| {
        let row = option.group.as_ref()?.resource?;
        let rank = [
            COMMAND_TEMPLATE_PREFIX,
            COMMAND_PATTERN_PREFIX,
            COMMAND_EXACT_PREFIX,
        ]
        .iter()
        .position(|prefix| option.id.starts_with(prefix))?;
        Some((row, rank))
    };
    let mut defaults: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for (position, option) in options.iter().enumerate() {
        if let Some((row, rank)) = rank(option) {
            let best = defaults.entry(row).or_insert((rank, position));
            if rank < best.0 {
                *best = (rank, position);
            }
        }
    }
    for (position, option) in options.iter_mut().enumerate() {
        if let Some((row, _)) = rank(option) {
            option.is_default = defaults
                .get(&row)
                .is_some_and(|&(_, chosen)| chosen == position);
        }
    }
}

fn command_template_option(
    index: usize,
    exact: &PermissionRuleOption,
    observation: &CommandObservation,
    definition: &PatternDefinition,
    source_description: &str,
) -> Option<PermissionRuleOption> {
    let compiled = CompiledPattern::compile(definition).ok()?;
    if !compiled.matches(observation).ok()?.is_match() {
        return None;
    }
    let mut option = exact.clone();
    let [constraint] = option.rule.resources.as_mut_slice() else {
        return None;
    };
    constraint.selector = PermissionResourceSelector::CommandTemplate {
        definition: Box::new(definition.clone()),
    };
    constraint.attributes.remove(POSSIBLE_WORKDIRS_ATTRIBUTE);
    validate_command_templates(&option.rule).ok()?;
    option.id = format!(
        "{COMMAND_TEMPLATE_PREFIX}{}",
        canonical_json_sha256(&json!([index, compiled.fingerprint(), exact.rule]))
    );
    let workdir = exact
        .label
        .strip_prefix(EXACT_COMMAND_LABEL)
        .unwrap_or_default();
    option.label = format!("{}{workdir}", command_template_phrase(definition));
    option.description = format!(
        "{}. {COMMAND_TEMPLATE_EXECUTION_NOTICE}. Bound executable, workdir and argument structure; other shell resources require separate authority.",
        source_description.trim_end_matches('.'),
    );
    if let Some(group) = &mut option.group {
        group.value = definition.name.clone();
    }
    Some(option)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn rule_options(
    tool: &ToolKey,
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    resources: &[PermissionResource],
    input: &Value,
    input_digest: &str,
    cwd: &Path,
    authority: &PermissionAuthorityProfile,
) -> Vec<PermissionRuleOption> {
    let exact_arguments = PermissionArgumentConstraint::Exact {
        digest: input_digest.into(),
    };
    let resource_constraints = exact_resource_constraints(resources);
    let option = |id: &str,
                  label: &str,
                  description: &str,
                  effect: StructuredPermissionEffect,
                  resources: Vec<PermissionResourceConstraint>,
                  arguments: PermissionArgumentConstraint,
                  allowed_lifetimes: Vec<PermissionLifetime>,
                  broad: bool,
                  is_default: bool,
                  confirmation: Option<&str>| PermissionRuleOption {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        rule: StructuredPermissionRule {
            subject: subject.clone(),
            executor: executor.clone(),
            resources,
            arguments,
            lifetime: PermissionLifetime::Once,
            effect,
            family: None,
        },
        allowed_lifetimes,
        broad,
        is_default,
        confirmation: confirmation.map(String::from),
        group: None,
        caution: None,
        seen: None,
    };
    let reusable = vec![
        PermissionLifetime::Conversation,
        PermissionLifetime::Project,
        PermissionLifetime::Global,
    ];
    let mut exact_lifetimes = vec![PermissionLifetime::Once];
    exact_lifetimes.extend(reusable.iter().cloned());
    let exact_call_label = match (authority, resources.len()) {
        (PermissionAuthorityProfile::Shell, 1) => EXACT_COMMAND_LABEL,
        (PermissionAuthorityProfile::Shell, _) => EXACT_COMMANDS_LABEL,
        _ => EXACT_CALL_LABEL,
    };
    let mut options = vec![
        option(
            EXACT_CALL_OPTION,
            exact_call_label,
            "Allow only these exact arguments and resources.",
            StructuredPermissionEffect::Allow,
            resource_constraints.clone(),
            exact_arguments.clone(),
            exact_lifetimes,
            false,
            true,
            None,
        ),
        option(
            "deny_exact",
            exact_call_label,
            "Deny only these exact arguments and resources.",
            StructuredPermissionEffect::Deny,
            resource_constraints.clone(),
            exact_arguments,
            vec![PermissionLifetime::Project, PermissionLifetime::Global],
            false,
            false,
            None,
        ),
    ];

    // A mutating remote call is protected. Reusing its resources for any
    // arguments would skip the typed confirmation a local write demands, so it
    // earns only the exact call.
    if matches!(authority, PermissionAuthorityProfile::RemoteResource)
        && resources.iter().all(|resource| {
            remote_resource_identity(&resource.kind).is_some() && !resource.protected
        })
    {
        options.push(option(
            "allow_remote_resources",
            REMOTE_RESOURCES_LABEL,
            "Allow these authority-issued resources with different display controls.",
            StructuredPermissionEffect::Allow,
            resources
                .iter()
                .map(reusable_remote_resource_constraint)
                .collect(),
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            None,
        ));
    }

    if matches!(authority, PermissionAuthorityProfile::Url)
        && let [resource] = resources
        && resource.kind == PermissionResourceKind::Url
        && let Some(strict) = strict_http_url(&resource.value)
    {
        let exact_url = PermissionResourceConstraint {
            kind: PermissionResourceKind::Url,
            selector: PermissionResourceSelector::Digest {
                digest: resource_value_digest(&resource.value, &PermissionResourceKind::Url)
                    .expect("strict URL has a digest"),
            },
            access: resource.access.clone(),
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        options.push(option(
            "allow_exact_url",
            EXACT_PAGE_LABEL,
            "Allow this normalized URL with different fetch format or timeout controls.",
            StructuredPermissionEffect::Allow,
            vec![exact_url],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            None,
        ));

        let ladder = UrlLadder {
            subject,
            executor,
            access: resource.access.as_ref(),
            reusable: &reusable,
        };
        // Deepest first, dropping the origin's own root: the rung below reaches
        // exactly that far and says so in the origin's own terms. The deepest
        // rung is where the ladder starts.
        let roots = url_subtree_roots(&strict).unwrap_or_default();
        let pages = roots.len().saturating_sub(1).min(MAX_URL_LADDER_RUNGS);
        for (step, root) in roots[..pages].iter().enumerate() {
            options.push(ladder.rung(
                match step {
                    0 => URL_SUBTREE_OPTION_ID.into(),
                    step => format!("{URL_SUBTREE_OPTION_ID}_{step}"),
                },
                match step {
                    0 => PAGE_SUBTREE_LABEL.into(),
                    _ => format!("pages under {}/", url_place(root)),
                },
                format!("Allow requested URLs at or below {root}/**."),
                format!("{root}/**"),
                PermissionResourceSelector::UrlSubtreeDigest {
                    digest: url_subtree_digest(root),
                },
                step == 0,
            ));
        }

        let origin = strict.url.origin().ascii_serialization();
        options.push(ladder.rung(
            URL_ORIGIN_OPTION_ID.into(),
            format!("any page on {}", url_place(&origin)),
            format!("Allow any requested URL on {origin}/**."),
            format!("{origin}/**"),
            PermissionResourceSelector::UrlOriginDigest {
                digest: url_origin_digest(&resource.value).expect("strict URL has an origin"),
            },
            pages == 0,
        ));
        options.push(option(
            "allow_any_url",
            ANY_PAGE_LABEL,
            "Allow any public HTTP(S) URL accepted by this exact tool contract.",
            StructuredPermissionEffect::Allow,
            vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Url,
                selector: PermissionResourceSelector::Any,
                access: resource.access.clone(),
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            Some("ALLOW ANY URL"),
        ));
    }

    if matches!(authority, PermissionAuthorityProfile::Query)
        && let [resource] = resources
        && resource.kind == PermissionResourceKind::Query
    {
        options.push(option(
            "allow_exact_query",
            EXACT_QUERY_LABEL,
            "Allow this query with different result limits or paging controls.",
            StructuredPermissionEffect::Allow,
            exact_resource_constraints(resources),
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            true,
            None,
        ));
        options.push(option(
            "allow_any_query",
            ANY_QUERY_LABEL,
            "Allow any future query through this exact tool contract.",
            StructuredPermissionEffect::Allow,
            vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Query,
                selector: PermissionResourceSelector::Any,
                access: resource.access.clone(),
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            Some("ALLOW ANY SEARCH"),
        ));
    }

    if matches!(authority, PermissionAuthorityProfile::Shell)
        && !resources.is_empty()
        && resources
            .iter()
            .all(|resource| resource.kind == PermissionResourceKind::Command)
    {
        let workdir_label = workdir_label(resources.first());
        let project = normalized_filesystem_path(&cwd.to_string_lossy());
        let project = project.as_deref();
        let workdirs = resources
            .iter()
            .filter_map(|resource| resource.attributes.get(WORKDIR_ATTRIBUTE))
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let shared_suffix = if workdirs.len() == 1 {
            workdir_suffix(workdirs.first().copied(), project)
        } else {
            String::new()
        };
        add_command_options(
            &mut options,
            resources,
            subject,
            executor,
            &reusable,
            project,
        );
        // Protected commands were reviewed as whole command lines because analysis
        // dropped operands, so only the blanket options below describe them
        // truthfully.
        if resources.iter().all(|resource| !resource.protected) {
            options.push(option(
                EXACT_COMMANDS_OPTION,
                &format!(
                    "{}{shared_suffix}",
                    if resources.len() == 1 {
                        EXACT_COMMAND_LABEL
                    } else {
                        EXACT_COMMANDS_LABEL
                    }
                ),
                &format!(
                    "Allow {} in {workdir_label} with different timeout or display controls.",
                    listed_commands(resources.iter().map(|resource| resource.value.as_str()))
                ),
                StructuredPermissionEffect::Allow,
                exact_resource_constraints(resources),
                PermissionArgumentConstraint::Unconstrained,
                reusable.clone(),
                true,
                false,
                None,
            ));
            let mut patterns = Vec::new();
            let mut exact_fallbacks = Vec::new();
            let pattern_constraints = exact_resource_constraints(resources)
                .into_iter()
                .zip(resources)
                .map(|(mut constraint, resource)| {
                    if let Some(pattern) = reusable_prefix(&resource.value) {
                        if !patterns.contains(&pattern) {
                            patterns.push(pattern.clone());
                        }
                        constraint.selector =
                            PermissionResourceSelector::CommandPattern { pattern };
                    } else {
                        exact_fallbacks.push(resource.value.as_str());
                    }
                    constraint
                })
                .collect();
            if !patterns.is_empty() {
                let label = if patterns.len() == 1 && exact_fallbacks.is_empty() {
                    format!("{}{shared_suffix}", safe_summary(&patterns[0]))
                } else {
                    format!("{COMMAND_PATTERNS_LABEL}{shared_suffix}")
                };
                let summaries = patterns
                    .iter()
                    .map(|pattern| safe_summary(pattern))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut description = format!(
                    "Allow commands matching these patterns in {workdir_label}: {summaries}."
                );
                if !exact_fallbacks.is_empty() {
                    description.push_str(&format!(
                        " Also allow {} exactly as reviewed.",
                        listed_commands(exact_fallbacks.iter().copied())
                    ));
                }
                options.push(option(
                    COMMAND_PATTERNS_OPTION,
                    &label,
                    &description,
                    StructuredPermissionEffect::Allow,
                    pattern_constraints,
                    PermissionArgumentConstraint::Unconstrained,
                    reusable.clone(),
                    true,
                    false,
                    None,
                ));
            }
        }
        // One constraint per distinct workdir. Every attribute of a constraint
        // must match for it to cover a resource, so pinning only the first
        // workdir would leave `cd /elsewhere && …` uncoverable and the answer
        // uncommittable.
        if !workdirs.is_empty() {
            options.push(option(
                "allow_commands_in_workdir",
                &match (workdirs.len(), shared_suffix.is_empty()) {
                    (1, true) => PROJECT_COMMANDS_LABEL.to_owned(),
                    (1, false) => format!("any command{shared_suffix}"),
                    _ => WORKDIRS_COMMANDS_LABEL.to_owned(),
                },
                &format!(
                    "Allow arbitrary commands starting in {}.",
                    listed_commands(workdirs.iter().copied())
                ),
                StructuredPermissionEffect::Allow,
                workdirs
                    .iter()
                    .map(|workdir| PermissionResourceConstraint {
                        kind: PermissionResourceKind::Command,
                        selector: PermissionResourceSelector::Any,
                        access: Some(PermissionResourceAccess::Execute),
                        protected: None,
                        attributes: BTreeMap::from([(
                            WORKDIR_ATTRIBUTE.into(),
                            PermissionResourceSelector::Digest {
                                digest: pinned_digest(workdir, &PermissionResourceKind::Directory),
                            },
                        )]),
                    })
                    .collect(),
                PermissionArgumentConstraint::Unconstrained,
                reusable.clone(),
                true,
                false,
                Some(BROAD_SHELL_PHRASE),
            ));
        }
        options.push(option(
            "allow_any_command",
            ANY_COMMAND_LABEL,
            "Allow arbitrary shell commands from any working directory.",
            StructuredPermissionEffect::Allow,
            vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Command,
                selector: PermissionResourceSelector::Any,
                access: Some(PermissionResourceAccess::Execute),
                protected: None,
                attributes: BTreeMap::new(),
            }],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            Some(BROAD_SHELL_PHRASE),
        ));
    }

    add_filesystem_options(
        &mut options,
        resources,
        input,
        cwd,
        subject,
        executor,
        &reusable,
        authority,
    );
    if tool.is_mcp() {
        options.push(option(
            "allow_whole_mcp_tool_conversation",
            ANY_ARGUMENTS_LABEL,
            "Broad: allow this MCP tool with any arguments for the conversation.",
            StructuredPermissionEffect::Allow,
            resource_constraints,
            PermissionArgumentConstraint::Unconstrained,
            vec![PermissionLifetime::Conversation],
            true,
            false,
            Some("ALLOW MCP TOOL"),
        ));
    }
    settle_row_defaults(&mut options);
    let laddered = options
        .iter()
        .any(|option| option.is_default && option.id != EXACT_CALL_OPTION);
    if let Some(exact) = options
        .iter_mut()
        .find(|option| option.id == EXACT_CALL_OPTION)
    {
        exact.is_default = !laddered;
    }
    options
}

/// How a workdir reads in a sentence, or a stand-in when the resource carries
/// none.
pub(super) fn workdir_label(resource: Option<&PermissionResource>) -> String {
    resource
        .and_then(|resource| resource.attributes.get(WORKDIR_ATTRIBUTE))
        .map_or_else(
            || "this workdir".to_owned(),
            |workdir| safe_summary(workdir),
        )
}

/// ` in caudra-agent/` for a command that starts somewhere other than the
/// project root, and nothing for one that starts there.
fn workdir_suffix(workdir: Option<&str>, project: Option<&Path>) -> String {
    workdir
        .and_then(normalized_filesystem_path)
        .and_then(|workdir| {
            folder_place(&workdir, project, caudra_storage::paths::home().as_deref())
        })
        .map(|place| format!(" in {place}"))
        .unwrap_or_default()
}

/// How a folder reads in a scope: relative inside the project, `~`-abbreviated
/// under home, absolute elsewhere, always ending in a slash. The project itself
/// has no place of its own, so the caller says what it means there.
fn folder_place(folder: &Path, project: Option<&Path>, home: Option<&Path>) -> Option<String> {
    if project == Some(folder) {
        return None;
    }
    let place =
        if let Some(relative) = project.and_then(|project| folder.strip_prefix(project).ok()) {
            relative.to_string_lossy().into_owned()
        } else if let Some(relative) = home.and_then(|home| folder.strip_prefix(home).ok()) {
            format!("{HOME_PREFIX}{}", relative.to_string_lossy())
        } else {
            folder.to_string_lossy().into_owned()
        };
    Some(format!("{}/", safe_summary(place.trim_end_matches('/'))))
}

/// A URL as a scope names it: the scheme only when it is not https.
fn url_place(url: &str) -> &str {
    url.strip_prefix(HTTPS_SCHEME).unwrap_or(url)
}

/// One ladder per command, so a request that batches several can be remembered
/// at a different breadth for each.
///
/// A ladder climbs from the command itself through its ancestor prefixes, the
/// reusable prefix among them as the default, out to the bare executable,
/// which is graded broad as a typed one is. Anything wider is a claim about
/// the whole request and stays in the blanket options. Rungs leave the
/// arguments unconstrained because a composition cannot pin them for one
/// resource and not another.
pub(super) fn add_command_options(
    options: &mut Vec<PermissionRuleOption>,
    resources: &[PermissionResource],
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    reusable: &[PermissionLifetime],
    project: Option<&Path>,
) {
    for (index, resource) in resources.iter().enumerate() {
        if resource.protected {
            continue;
        }
        let constraint = resource_constraint(resource);
        let workdir = workdir_label(Some(resource));
        let suffix = workdir_suffix(
            resource
                .attributes
                .get(WORKDIR_ATTRIBUTE)
                .map(String::as_str),
            project,
        );
        let command = safe_summary(&resource.value);
        let rung = |id: String,
                    label: String,
                    value: &str,
                    description: String,
                    selector: PermissionResourceSelector| PermissionRuleOption {
            id,
            label,
            description,
            rule: StructuredPermissionRule {
                subject: subject.clone(),
                executor: executor.clone(),
                resources: vec![PermissionResourceConstraint {
                    selector,
                    ..constraint.clone()
                }],
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: PermissionLifetime::Once,
                effect: StructuredPermissionEffect::Allow,
                family: None,
            },
            allowed_lifetimes: reusable.to_vec(),
            broad: true,
            is_default: false,
            confirmation: None,
            group: Some(PermissionOptionGroup {
                key: format!("{COMMAND_GROUP_PREFIX}{index}"),
                value: value.to_owned(),
                resource: Some(index),
            }),
            caution: None,
            seen: None,
        };
        options.push(rung(
            format!("{COMMAND_EXACT_PREFIX}{index}"),
            format!("{EXACT_COMMAND_LABEL}{suffix}"),
            EXACT_COMMAND_CHIP,
            format!("Allow `{command}` in {workdir} with different timeout or display controls."),
            constraint.selector.clone(),
        ));
        let default = reusable_prefix(&resource.value);
        let mut patterns: Vec<String> = ancestor_prefixes(&resource.value)
            .into_iter()
            .filter(|pattern| Some(pattern) != default.as_ref())
            .chain(default.clone())
            .collect();
        patterns.sort_by_key(|pattern| Reverse(pattern.split_whitespace().count()));
        for pattern in patterns {
            let id = if Some(&pattern) == default.as_ref() {
                format!("{COMMAND_PATTERN_PREFIX}{index}")
            } else {
                let literals = pattern.split_whitespace().count() - 1;
                format!("{COMMAND_PREFIX_PREFIX}{index}_{literals}")
            };
            let confirmation = grade_command_pattern(&pattern, &resource.value)
                .ok()
                .and_then(|grade| grade.confirmation);
            options.push(PermissionRuleOption {
                confirmation: confirmation.map(String::from),
                ..rung(
                    id,
                    format!("{}{suffix}", safe_summary(&pattern)),
                    &pattern,
                    format!(
                        "Allow commands matching `{}` in {workdir}.",
                        safe_summary(&pattern)
                    ),
                    PermissionResourceSelector::CommandPattern {
                        pattern: pattern.clone(),
                    },
                )
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn add_filesystem_options(
    options: &mut Vec<PermissionRuleOption>,
    resources: &[PermissionResource],
    input: &Value,
    cwd: &Path,
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    reusable: &[PermissionLifetime],
    authority: &PermissionAuthorityProfile,
) {
    if resources.is_empty()
        || resources.iter().any(|resource| {
            !matches!(
                resource.kind,
                PermissionResourceKind::File | PermissionResourceKind::Directory
            )
        })
    {
        return;
    }
    let PermissionAuthorityProfile::Filesystem { input_pointers } = authority else {
        return;
    };
    if resources
        .iter()
        .any(|resource| resource.access == Some(PermissionResourceAccess::List))
    {
        add_browse_options(options, resources, subject, executor, reusable);
        return;
    }
    let write = resources
        .iter()
        .any(|resource| resource.access == Some(PermissionResourceAccess::Write));
    let search = !input_pointers.is_empty();

    let arguments = if search {
        let Ok(digest) = selected_input_digest(input, input_pointers) else {
            return;
        };
        PermissionArgumentConstraint::SelectedDigest {
            pointers: input_pointers.clone(),
            digest,
        }
    } else {
        PermissionArgumentConstraint::Unconstrained
    };
    let exact_label = if search {
        EXACT_SEARCH_LABEL
    } else {
        match resources {
            [resource] if resource.kind == PermissionResourceKind::File => EXACT_FILE_LABEL,
            [_] => EXACT_FOLDER_LABEL,
            _ if resources
                .iter()
                .all(|resource| resource.kind == PermissionResourceKind::File) =>
            {
                EXACT_FILES_LABEL
            }
            _ => EXACT_PATHS_LABEL,
        }
    };
    options.push(PermissionRuleOption {
        id: "allow_exact_resources".into(),
        label: exact_label.into(),
        description: if write {
            "Allow future changes to only the exact reviewed path set.".into()
        } else if search {
            "Allow the same search expression at this exact root.".into()
        } else {
            "Allow future reads of only the exact reviewed path set.".into()
        },
        rule: StructuredPermissionRule {
            subject: subject.clone(),
            executor: executor.clone(),
            resources: exact_resource_constraints(resources),
            arguments,
            lifetime: PermissionLifetime::Once,
            effect: StructuredPermissionEffect::Allow,
            family: None,
        },
        allowed_lifetimes: reusable.to_vec(),
        broad: true,
        is_default: true,
        confirmation: write.then(|| "ALLOW FILE CHANGES".into()),
        group: Some(PermissionOptionGroup {
            key: SUBTREE_OPTION_ID.into(),
            value: exact_label.into(),
            resource: None,
        }),
        caution: None,
        seen: None,
    });

    // A protected path never earns a subtree grant, because every rung of the
    // ladder below pins `protected: Some(false)` and so could not cover it
    // anyway. Returning here keeps the exact-path option above, which carries
    // the real protected flag and is reusable across differing tool inputs.
    if resources.iter().any(|resource| resource.protected) {
        return;
    }

    // Only a first-party read earns a widened subtree grant. A write request
    // keeps its exact subject, so `allow_filesystem_subtree` on a write never
    // becomes reachable from a reading contract.
    let family = (is_filesystem_read_subject(subject)
        && resources.iter().all(|resource| {
            is_filesystem_read_kind(&resource.kind)
                && is_filesystem_read_access(resource.access.as_ref())
        }))
    .then_some(PermissionCapabilityFamily::FilesystemRead);

    let mut roots = Vec::with_capacity(resources.len());
    let mut constraints = Vec::with_capacity(resources.len());
    for resource in resources {
        let Some(path) = normalized_filesystem_path(&resource.value) else {
            return;
        };
        let root = if resource.kind == PermissionResourceKind::File {
            path.parent().unwrap_or(&path)
        } else {
            path.as_path()
        };
        roots.push(root.to_path_buf());
        constraints.push(PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: filesystem_subtree_digest(&root.to_string_lossy())
                    .expect("normalized filesystem root has a digest"),
            },
            access: resource.access.clone(),
            protected: Some(false),
            attributes: BTreeMap::new(),
        });
    }
    roots.sort();
    roots.dedup();

    let project = normalized_filesystem_path(&cwd.to_string_lossy());
    let ladder = SubtreeLadder {
        family,
        write,
        project: project.as_deref(),
        repository: enclosing_repository(roots.first().map(PathBuf::as_path)),
        home: caudra_storage::paths::home(),
    };
    options.push(ladder.rung(
        SUBTREE_OPTION_ID.into(),
        &roots,
        constraints,
        subject,
        executor,
        reusable,
    ));

    // Every rung above the first is a single root the whole request fits
    // under, so the ladder walks the ancestors of what the roots have in
    // common. With one root that common ancestor is the root itself, whose own
    // level is already the rung above, so the walk starts one step up.
    let Some(common) = common_ancestor(&roots) else {
        return;
    };
    let climb: Vec<_> = if roots.len() == 1 {
        common.ancestors().skip(1).collect()
    } else {
        common.ancestors().collect()
    };
    for (step, ancestor) in climb.into_iter().enumerate() {
        let Some(digest) = filesystem_subtree_digest(&ancestor.to_string_lossy()) else {
            continue;
        };
        let mut widened: Vec<PermissionResourceConstraint> = Vec::new();
        for resource in resources {
            if widened.iter().any(|constraint| {
                constraint.kind == resource.kind && constraint.access == resource.access
            }) {
                continue;
            }
            widened.push(PermissionResourceConstraint {
                kind: resource.kind.clone(),
                selector: PermissionResourceSelector::FilesystemSubtreeDigest {
                    digest: digest.clone(),
                },
                access: resource.access.clone(),
                protected: Some(false),
                attributes: BTreeMap::new(),
            });
        }
        options.push(ladder.rung(
            format!("{SUBTREE_OPTION_ID}_{}", step + 1),
            std::slice::from_ref(&ancestor.to_path_buf()),
            widened,
            subject,
            executor,
            reusable,
        ));
    }
}

fn add_browse_options(
    options: &mut Vec<PermissionRuleOption>,
    resources: &[PermissionResource],
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    reusable: &[PermissionLifetime],
) {
    let [resource] = resources else {
        return;
    };
    if !is_filesystem_browse_subject(subject)
        || *executor != PermissionExecutorKind::Native
        || resource.kind != PermissionResourceKind::Directory
    {
        return;
    }
    let Some(recursion) = resource.attributes.get(BROWSE_RECURSION_ATTRIBUTE) else {
        return;
    };
    if !matches!(recursion.as_str(), BROWSE_DIRECT | BROWSE_RECURSIVE) {
        return;
    }
    let Some(digest) = filesystem_subtree_digest(&resource.value) else {
        return;
    };
    for recursive in [false, true] {
        if !recursive && recursion == BROWSE_RECURSIVE
            || recursive && resource.protected && recursion == BROWSE_DIRECT
        {
            continue;
        }
        let label = if recursive {
            BROWSE_NAMES_LABEL
        } else {
            EXACT_FOLDER_LABEL
        };
        let mut constraint = resource_constraint(resource);
        if recursive && !resource.protected {
            constraint.selector = PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: digest.clone(),
            };
            constraint.attributes.insert(
                BROWSE_RECURSION_ATTRIBUTE.into(),
                PermissionResourceSelector::Digest {
                    digest: pinned_digest(
                        BROWSE_RECURSIVE,
                        &PermissionResourceKind::Custom {
                            name: BROWSE_RECURSION_ATTRIBUTE.into(),
                        },
                    ),
                },
            );
        }
        options.push(PermissionRuleOption {
            id: if recursive { BROWSE_SUBTREE_OPTION_ID } else { BROWSE_OPTION_ID }.into(),
            label: label.into(),
            description: if recursive {
                "Allow recursive filename enumeration below this root, with any filename pattern; never file contents."
            } else {
                "Allow names in this exact directory only; not nested directories or file contents."
            }.into(),
            rule: StructuredPermissionRule {
                subject: subject.clone(),
                executor: executor.clone(),
                resources: vec![constraint],
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: PermissionLifetime::Once,
                effect: StructuredPermissionEffect::Allow,
                family: Some(PermissionCapabilityFamily::FilesystemBrowse),
            },
            allowed_lifetimes: reusable.to_vec(),
            broad: true,
            is_default: !recursive || recursion == BROWSE_RECURSIVE,
            confirmation: None,
            group: Some(PermissionOptionGroup {
                key: BROWSE_OPTION_ID.into(),
                value: label.into(),
                resource: None,
            }),
            caution: None,
            seen: None,
        });
    }
}

/// What every rung of a URL ladder shares. The rungs are one authority at
/// widening reach — the page a request named, each prefix of its path, and the
/// origin — so they carry one group key and the prompt walks them as one row.
pub(super) struct UrlLadder<'a> {
    pub(super) subject: &'a PermissionSubject,
    pub(super) executor: &'a PermissionExecutorKind,
    pub(super) access: Option<&'a PermissionResourceAccess>,
    pub(super) reusable: &'a [PermissionLifetime],
}

impl UrlLadder<'_> {
    pub(super) fn rung(
        &self,
        id: String,
        label: String,
        description: String,
        pattern: String,
        selector: PermissionResourceSelector,
        is_default: bool,
    ) -> PermissionRuleOption {
        PermissionRuleOption {
            id,
            label,
            description,
            rule: StructuredPermissionRule {
                subject: self.subject.clone(),
                executor: self.executor.clone(),
                resources: vec![PermissionResourceConstraint {
                    kind: PermissionResourceKind::Url,
                    selector,
                    access: self.access.cloned(),
                    protected: Some(false),
                    attributes: BTreeMap::new(),
                }],
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: PermissionLifetime::Once,
                effect: StructuredPermissionEffect::Allow,
                family: None,
            },
            allowed_lifetimes: self.reusable.to_vec(),
            broad: true,
            is_default,
            confirmation: None,
            group: Some(PermissionOptionGroup {
                key: URL_SUBTREE_OPTION_ID.into(),
                value: pattern,
                resource: None,
            }),
            caution: None,
            seen: None,
        }
    }
}

/// The context a subtree rung is judged in, so every rung is labelled, described
/// and cautioned by one rule instead of by the loop that happens to build it.
pub(super) struct SubtreeLadder<'a> {
    pub(super) family: Option<PermissionCapabilityFamily>,
    pub(super) write: bool,
    pub(super) project: Option<&'a Path>,
    pub(super) repository: Option<PathBuf>,
    pub(super) home: Option<PathBuf>,
}

impl SubtreeLadder<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn rung(
        &self,
        id: String,
        roots: &[PathBuf],
        resources: Vec<PermissionResourceConstraint>,
        subject: &PermissionSubject,
        executor: &PermissionExecutorKind,
        reusable: &[PermissionLifetime],
    ) -> PermissionRuleOption {
        let patterns = roots
            .iter()
            .map(|root| format!("{}/**", root.to_string_lossy().trim_end_matches('/')))
            .collect::<Vec<_>>()
            .join(", ");
        let caution = self.caution(roots);
        let mut value = patterns.clone();
        if roots.len() == 1 && self.project == Some(roots[0].as_path()) {
            value.push_str(" (project root)");
        }
        let mut description = if self.family.is_some() {
            format!("Allow reading, listing, and searching any path below {patterns}.")
        } else {
            format!("Allow matching paths below {patterns}.")
        };
        if let Some(reason) = self.reason(caution) {
            description.push(' ');
            description.push_str(&reason);
        }
        PermissionRuleOption {
            id,
            label: match roots {
                [root] => self.place(root),
                _ => SUBTREES_LABEL.into(),
            },
            description,
            rule: StructuredPermissionRule {
                subject: subject.clone(),
                executor: executor.clone(),
                resources,
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: PermissionLifetime::Once,
                effect: StructuredPermissionEffect::Allow,
                family: self.family,
            },
            allowed_lifetimes: reusable.to_vec(),
            broad: true,
            is_default: false,
            confirmation: self.confirmation(caution),
            group: Some(PermissionOptionGroup {
                key: SUBTREE_OPTION_ID.into(),
                value,
                resource: None,
            }),
            caution,
            seen: None,
        }
    }

    /// How much a rung exposes, measured against the two landmarks a user
    /// reasons about. Grave once the grant swallows the home directory, which a
    /// root outside home entirely does not do — that root is beside home, not
    /// above it, and reaches nothing home holds. An unknown home is grave,
    /// because a boundary that cannot be found cannot be respected.
    pub(super) fn caution(&self, roots: &[PathBuf]) -> Option<PermissionCaution> {
        let swallows_home = self.home.as_deref().is_none_or(|home| {
            roots
                .iter()
                .any(|root| home == root || home.starts_with(root))
        });
        if swallows_home {
            return Some(PermissionCaution::Danger);
        }
        let outside_repository = self.repository.as_deref().is_none_or(|repository| {
            roots
                .iter()
                .any(|root| root != repository && !root.starts_with(repository))
        });
        outside_repository.then_some(PermissionCaution::Warn)
    }

    fn place(&self, root: &Path) -> String {
        if root.parent().is_none() {
            return FILESYSTEM_ROOT_LABEL.into();
        }
        folder_place(root, self.project, self.home.as_deref())
            .unwrap_or_else(|| PROJECT_LABEL.into())
    }

    pub(super) fn reason(&self, caution: Option<PermissionCaution>) -> Option<String> {
        match caution? {
            PermissionCaution::Danger => Some("This takes in your whole home directory.".into()),
            PermissionCaution::Warn => Some(match &self.repository {
                Some(repository) => format!(
                    "This reaches outside the repository at {}.",
                    repository.display()
                ),
                None => "This reaches outside any repository.".into(),
            }),
        }
    }

    /// The phrase to type before the grant is stored. Reaching past home is the
    /// graver claim, so it names itself even when the grant also writes.
    pub(super) fn confirmation(&self, caution: Option<PermissionCaution>) -> Option<String> {
        if caution == Some(PermissionCaution::Danger) {
            return Some(OUTSIDE_HOME_PHRASE.into());
        }
        self.write.then(|| "ALLOW DIRECTORY CHANGES".into())
    }
}

/// The deepest directory every root sits under, which is the first rung the
/// whole request can share.
pub(super) fn common_ancestor(roots: &[PathBuf]) -> Option<PathBuf> {
    let mut shared = roots.first()?.clone();
    for root in &roots[1..] {
        while !root.starts_with(&shared) {
            if !shared.pop() {
                return Some(shared);
            }
        }
    }
    Some(shared)
}

/// The nearest ancestor holding a `.git` entry, which is what "outside the
/// repository" is measured against. A worktree or submodule records a file
/// rather than a directory, so existence is the test.
pub(super) fn enclosing_repository(start: Option<&Path>) -> Option<PathBuf> {
    start?
        .ancestors()
        .find(|ancestor| ancestor.join(GIT_METADATA_DIR).exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod learned_scope_tests {
    use super::{
        COMMAND_EXACT_PREFIX, COMMAND_PATTERN_PREFIX, COMMAND_TEMPLATE_PREFIX,
        IMPORTED_PATTERN_CONTEXT, command_template_option,
    };
    use crate::permissions::{
        COMMAND_OBSERVATION_ATTRIBUTE, COMMAND_OBSERVATION_BINDING_ATTRIBUTE, ComposedAnswerError,
        ComposedRow, PermissionExecutorKind, PermissionLifetime, PermissionRequest,
        PermissionResourceSelector, PermissionRowGrant, PermissionRuleOption, ResourceCoverage,
        RuleOrigin, StructuredPermissionDecision, StructuredPermissionRule,
        evaluate_structured_permission_rules,
        pattern_recognition::{
            CommandObservation, InvocationOutcome, ObservationProvenance, PatternCandidate,
            PatternRecognizer, RecognizerLimits, fixtures,
        },
        permission_rule_covers_request, prepared_command_binding,
        structured::{POSSIBLE_WORKDIRS_ATTRIBUTE, trusted_command_observation},
        tests::{SHELL_WORKDIR, shell_intent, workcell_shell_subject},
    };
    use caudra_config::ToolKey;
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, PatternDefinition, SlotCombinations,
    };
    use serde_json::json;
    use std::path::Path;
    use test_case::test_case;

    const REQUEST_ID: &str = "learned-pattern-request";
    const PACKAGE: &str = "core";
    const OTHER_PACKAGE: &str = "other";
    const UNOBSERVED_PACKAGE: &str = "new";
    const OTHER_OPERATION: &str = "build";
    const LEARNED_SOURCE: &str = "Native observations: 2 observations across 2 sessions";
    const IMPORTED_SOURCE: &str = "Imported history (unverified): 2 observations across 2 sessions";
    const REQUESTED_OUTCOMES: &str = "2 requested (execution outcome not recorded)";
    const UNKNOWN_OUTCOMES: &str = "2 unknown outcomes";
    const COVERED_REASON: &str = "already allowed";

    fn observation(package: &str) -> CommandObservation {
        let mut observation =
            fixtures::observation(&["cargo", "check", "-p", package, "--tests"], package);
        observation.roles = vec![
            ArgumentRole::Executable,
            ArgumentRole::Operation,
            ArgumentRole::Flag,
            ArgumentRole::Unknown,
            ArgumentRole::Flag,
        ];
        observation.context.executable_identity = "cargo".into();
        observation.context.effective_workdir = SHELL_WORKDIR.into();
        observation.context.path_binding = SHELL_WORKDIR.into();
        observation
    }

    fn learned_candidates() -> Vec<PatternCandidate> {
        let mut recognizer =
            PatternRecognizer::new(RecognizerLimits::default(), fixtures::NOW_MS).unwrap();
        for package in [PACKAGE, OTHER_PACKAGE] {
            recognizer.observe(observation(package)).unwrap();
        }
        recognizer.suggestions().unwrap()
    }

    fn prepared(mut observation: CommandObservation) -> PermissionRequest {
        let command = observation.argv.join(" ");
        let input = json!({"command": command, "workdir": SHELL_WORKDIR});
        let mut intent = shell_intent(&[&command]);
        let resource = &mut intent.resources[0];
        observation.source.input_hash = prepared_command_binding(&resource.value, &input);
        resource.attributes.insert(
            COMMAND_OBSERVATION_BINDING_ATTRIBUTE.into(),
            observation.source.input_hash.clone(),
        );
        resource.attributes.insert(
            COMMAND_OBSERVATION_ATTRIBUTE.into(),
            serde_json::to_string(&observation).unwrap(),
        );
        resource.attributes.insert(
            POSSIBLE_WORKDIRS_ATTRIBUTE.into(),
            json!({"kind": "known", "symbolic_paths": [SHELL_WORKDIR]}).to_string(),
        );
        PermissionRequest::from_intent_with_identity(
            REQUEST_ID.into(),
            ToolKey::native("shell"),
            &intent,
            input,
            Path::new(SHELL_WORKDIR),
            workcell_shell_subject(),
            PermissionExecutorKind::Native,
        )
    }

    fn template_option(request: &PermissionRequest) -> &PermissionRuleOption {
        request
            .options
            .iter()
            .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
            .unwrap()
    }

    fn template_definition(option: &PermissionRuleOption) -> PatternDefinition {
        let PermissionResourceSelector::CommandTemplate { definition } =
            &option.rule.resources[0].selector
        else {
            panic!("expected a command template")
        };
        (**definition).clone()
    }

    #[test_case(&["nimblectl", "inspect"]; "arbitrary_cli")]
    #[test_case(&["forge", "verify"]; "another_arbitrary_cli")]
    #[test_case(&["cargo", "check"]; "cargo_check")]
    #[test_case(&["cargo", "test"]; "cargo_test")]
    #[test_case(&["cargo", "clippy"]; "cargo_clippy")]
    #[test_case(&["cargo", "nextest", "run"]; "cargo_nextest")]
    #[test_case(&["just", "check"]; "just_check")]
    #[test_case(&["just", "lint"]; "just_lint")]
    #[test_case(&["just", "test"]; "just_test")]
    fn templates_require_support_without_a_catalog_fallback(argv: &[&str]) {
        let limits = RecognizerLimits::default();
        let min_support = limits.min_support;
        let mut recognizer = PatternRecognizer::new(limits, fixtures::NOW_MS).unwrap();
        let mut fact = fixtures::observation(argv, REQUEST_ID);
        fact.roles[1..].fill(ArgumentRole::Operation);
        fact.context.executable_identity = argv[0].into();
        fact.context.effective_workdir = SHELL_WORKDIR.into();
        fact.context.path_binding = SHELL_WORKDIR.into();
        let mut request = prepared(fact.clone());
        for count in 0..=min_support {
            let candidates = recognizer.suggestions().unwrap();
            let expected = usize::from(count == min_support);
            assert_eq!(candidates.len(), expected);
            request.add_pattern_candidates(&candidates.into(), &[None]);
            assert_eq!(
                request
                    .options
                    .iter()
                    .filter(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
                    .count(),
                expected,
            );
            if count < min_support {
                fact.source.observation_id = format!("{REQUEST_ID}-{count}");
                fact.source.session_id = fact.source.observation_id.clone();
                recognizer.observe(fact.clone()).unwrap();
            }
        }
        assert!(template_option(&request).is_default);
        request.add_pattern_candidates(&Default::default(), &[None]);
        assert!(
            request
                .options
                .iter()
                .all(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
        );
        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option.is_default)
                .count(),
            1
        );
    }

    #[test_case(PermissionLifetime::Conversation; "conversation_proposal")]
    #[test_case(PermissionLifetime::Project; "project_proposal")]
    fn learned_scope_never_grants_itself(lifetime: PermissionLifetime) {
        let mut request = prepared(observation(PACKAGE));
        let candidates = learned_candidates().into();
        request.add_pattern_candidates(&candidates, &[None]);
        let option = template_option(&request);
        assert!(option.description.starts_with(LEARNED_SOURCE));
        assert_eq!(option.group.as_ref().unwrap().resource, Some(0));
        assert_eq!(
            template_definition(option).slots[0].domain,
            ArgumentDomain::ObservedSet {
                values: [PACKAGE.into(), OTHER_PACKAGE.into()].into()
            }
        );
        assert_eq!(
            evaluate_structured_permission_rules(&[], &request),
            StructuredPermissionDecision::NoMatch
        );
        let selected = request.option_rule(&option.id, lifetime).unwrap();
        assert!(permission_rule_covers_request(&selected, &request));
        assert!(permission_rule_covers_request(
            &selected,
            &prepared(observation(OTHER_PACKAGE))
        ));
        assert!(!permission_rule_covers_request(
            &selected,
            &prepared(observation(UNOBSERVED_PACKAGE))
        ));
        assert_eq!(
            evaluate_structured_permission_rules(&[], &request),
            StructuredPermissionDecision::NoMatch
        );
        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option.is_default)
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            [option.id.as_str()]
        );
        let id = option.id.clone();
        request.add_pattern_candidates(&candidates, &[None]);
        assert_eq!(template_option(&request).id, id);
        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
                .count(),
            1
        );
    }

    fn defaults(request: &PermissionRequest) -> Vec<String> {
        request
            .options
            .iter()
            .filter(|option| option.is_default)
            .map(|option| option.id.clone())
            .collect()
    }

    #[test]
    fn shell_default_rung_prefers_template_then_prefix() {
        let prefix = format!("{COMMAND_PATTERN_PREFIX}0");
        let mut request = prepared(observation(PACKAGE));
        assert_eq!(defaults(&request), [prefix.as_str()]);

        request.add_pattern_candidates(&learned_candidates().into(), &[None]);
        assert_eq!(defaults(&request), [template_option(&request).id.as_str()]);

        request.add_pattern_candidates(&Default::default(), &[None]);
        assert_eq!(defaults(&request), [prefix]);
    }

    /// `→` walks a row's ladder from narrow to wide, so every rung reaches the
    /// commands the rung before it reached, and more.
    #[test]
    fn right_arrow_always_widens() {
        let mut request = prepared(observation(PACKAGE));
        request.add_pattern_candidates(&learned_candidates().into(), &[None]);
        let mut other_operation = observation(PACKAGE);
        other_operation.argv[1] = OTHER_OPERATION.into();
        let fixtures: Vec<_> = [PACKAGE, OTHER_PACKAGE, UNOBSERVED_PACKAGE]
            .map(observation)
            .into_iter()
            .chain([other_operation])
            .map(prepared)
            .collect();
        let reach = |rule: &StructuredPermissionRule| {
            fixtures
                .iter()
                .map(|fixture| permission_rule_covers_request(rule, fixture))
                .collect::<Vec<_>>()
        };
        let ladder: Vec<_> = request
            .options
            .iter()
            .filter(|option| option.group.as_ref().and_then(|group| group.resource) == Some(0))
            .map(|option| {
                reach(
                    &request
                        .option_rule(&option.id, PermissionLifetime::Conversation)
                        .unwrap(),
                )
            })
            .collect();

        assert_eq!(
            ladder,
            [
                [true, false, false, false],
                [true, true, false, false],
                [true, true, true, false],
                [true, true, true, true]
            ]
        );
    }

    #[test_case("binding"; "missing_prepared_binding")]
    #[test_case("input"; "changed_prepared_input")]
    #[test_case("workdirs"; "multiple_effective_workdirs")]
    #[test_case("protected"; "protected_command")]
    #[test_case("prompt"; "requires_exact_prompt")]
    #[test_case("operation"; "unreviewed_operation")]
    #[test_case("flag"; "unreviewed_flag")]
    #[test_case("extra"; "extra_argv")]
    #[test_case("covered"; "already_covered")]
    fn learned_scope_requires_current_uncovered_bound_facts(change: &str) {
        let mut fact = observation(PACKAGE);
        match change {
            "operation" => fact.argv[1] = "publish".into(),
            "flag" => fact.argv[2] = "--manifest-path".into(),
            "extra" => {
                fact.argv.push("extra".into());
                fact.roles.push(ArgumentRole::Unknown);
            }
            _ => {}
        }
        let mut request = prepared(fact);
        match change {
            "binding" => {
                request.resources[0]
                    .attributes
                    .remove(COMMAND_OBSERVATION_BINDING_ATTRIBUTE);
            }
            "input" => request.input["command"] = "cargo publish".into(),
            "workdirs" => {
                request.resources[0].attributes.insert(
                    POSSIBLE_WORKDIRS_ATTRIBUTE.into(),
                    json!({"kind": "known", "symbolic_paths": [SHELL_WORKDIR, "/other"]})
                        .to_string(),
                );
            }
            "protected" => request.resources[0].protected = true,
            "prompt" => request.resources[0].requires_prompt = true,
            _ => {}
        }
        let coverage = (change == "covered").then(|| ResourceCoverage {
            origin: RuleOrigin::Conversation,
            authority: COVERED_REASON.into(),
            asks: false,
        });
        request.add_pattern_candidates(&learned_candidates().into(), &[coverage]);
        assert!(
            request
                .options
                .iter()
                .all(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
        );
    }

    #[test_case(ArgumentDomain::AnyLiteralArgument; "explicit_any_literal_edit")]
    #[test_case(ArgumentDomain::Regex { pattern: format!("{PACKAGE}|{UNOBSERVED_PACKAGE}") }; "explicit_regex_edit")]
    fn learned_domains_are_editable_without_changing_prepared_structure(domain: ArgumentDomain) {
        let mut request = prepared(observation(PACKAGE));
        request.add_pattern_candidates(&learned_candidates().into(), &[None]);
        let option = template_option(&request);
        let mut definition = template_definition(option);
        definition.slots[0].domain = domain;
        definition.combinations = SlotCombinations::Independent;
        let row = |definition| {
            [Some(ComposedRow {
                grant: PermissionRowGrant::Pattern {
                    option_id: option.id.clone(),
                    definition: Box::new(definition),
                },
                lifetime: PermissionLifetime::Conversation,
            })]
        };
        let reviewed = request.composed_rules(&row(definition.clone())).unwrap();
        assert!(permission_rule_covers_request(
            &reviewed[0],
            &prepared(observation(UNOBSERVED_PACKAGE))
        ));
        definition.context.path_binding = "/other".into();
        assert_eq!(
            request.composed_rules(&row(definition)),
            Err(ComposedAnswerError::TemplateNotOffered)
        );
    }

    #[test_case("renamed"; "labels_do_not_change_option_identity")]
    fn learned_option_identity_uses_constraints_not_labels(name: &str) {
        let request = prepared(observation(PACKAGE));
        let fact = trusted_command_observation(&request, &request.resources[0]).unwrap();
        let exact = request
            .options
            .iter()
            .find(|option| option.id == format!("{COMMAND_EXACT_PREFIX}0"))
            .unwrap();
        let candidate = learned_candidates().remove(0);
        let source = candidate.evidence.review_summary();
        let mut definition = candidate.definition;
        let fingerprint = definition.fingerprint().unwrap();
        let original = command_template_option(0, exact, &fact, &definition, &source).unwrap();
        definition.name = name.into();
        definition.slots[0].label = name.into();
        assert_eq!(definition.fingerprint().unwrap(), fingerprint);
        let renamed = command_template_option(0, exact, &fact, &definition, &source).unwrap();
        assert_eq!(original.id, renamed.id);
        assert_ne!(original.label, renamed.label);
    }

    #[test_case(OTHER_PACKAGE; "learned_evidence_is_retained")]
    fn learned_options_preserve_candidates_without_inventing_counts(other: &str) {
        let mut recognizer =
            PatternRecognizer::new(RecognizerLimits::default(), fixtures::NOW_MS).unwrap();
        for package in [PACKAGE, other] {
            recognizer.observe(observation(package)).unwrap();
        }
        let candidates = recognizer.suggestions().unwrap();
        assert_eq!(candidates.len(), 1);
        let mut request = prepared(observation(PACKAGE));
        request.add_pattern_candidates(&candidates.clone().into(), &[None]);
        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
                .count(),
            1
        );
        assert!(
            request
                .options
                .iter()
                .any(|option| option.description.starts_with(LEARNED_SOURCE))
        );
        assert_eq!(recognizer.suggestions().unwrap(), candidates);
    }

    #[test_case(ObservationProvenance::Native, InvocationOutcome::Requested, LEARNED_SOURCE, REQUESTED_OUTCOMES; "native_requests")]
    #[test_case(ObservationProvenance::Imported, InvocationOutcome::Unknown, IMPORTED_SOURCE, UNKNOWN_OUTCOMES; "imported_history")]
    fn learned_options_preserve_evidence_provenance_and_outcomes(
        provenance: ObservationProvenance,
        outcome: InvocationOutcome,
        source: &str,
        outcomes: &str,
    ) {
        let mut recognizer =
            PatternRecognizer::new(RecognizerLimits::default(), fixtures::NOW_MS).unwrap();
        for package in [PACKAGE, OTHER_PACKAGE] {
            let mut observation = observation(package);
            observation.source.provenance = provenance.clone();
            observation.source.outcome = outcome.clone();
            recognizer.observe(observation).unwrap();
        }
        let candidates = recognizer.suggestions().unwrap();
        let mut request = prepared(observation(PACKAGE));
        request.add_pattern_candidates(&candidates.clone().into(), &[None]);
        let learned = template_option(&request);
        assert!(learned.description.starts_with(source));
        assert!(learned.description.contains(outcomes));
        assert_eq!(
            learned.description.contains(IMPORTED_PATTERN_CONTEXT),
            provenance == ObservationProvenance::Imported,
        );
        assert_eq!(recognizer.suggestions().unwrap(), candidates);
    }
}

#[cfg(test)]
mod browse_tests {
    use super::{
        BROWSE_NAMES_LABEL, BROWSE_OPTION_ID, BROWSE_SUBTREE_OPTION_ID, EXACT_FOLDER_LABEL,
        rule_options,
    };
    use crate::permissions::structured::tests::{
        READ_CONTRACT, SOURCE_DIR, SUBTREE_OPTION, default_remote_identity, read_subtree_rule,
        workcell_request,
    };
    use crate::permissions::{
        PermissionAuthorityProfile, PermissionCapabilityFamily, PermissionExecutorKind,
        PermissionLifetime, PermissionRequest, PermissionResourceAccess, PermissionResourceKind,
        PermissionResourceSelector, PermissionRuleRecord, PermissionSubject,
        StructuredPermissionRule, permission_rule_covers_request, review::review_for_rule,
    };
    use caudra_storage::permission_state::{
        BROWSE_DIRECT, BROWSE_RECURSION_ATTRIBUTE, BROWSE_RECURSIVE,
    };
    use std::path::Path;
    use test_case::test_case;

    const GLOB_CONTRACT: &str = "file.glob.v1";
    const CHILD_DIR: &str = "/project/src/nested";
    const SIBLING_DIR: &str = "/project/src-other";
    const PROTECTED_DIR: &str = "/project/src/.ssh";

    fn browse_request(recursion: &str) -> PermissionRequest {
        let mut request = workcell_request(
            if recursion == BROWSE_DIRECT {
                READ_CONTRACT
            } else {
                GLOB_CONTRACT
            },
            PermissionResourceKind::Directory,
            PermissionResourceAccess::List,
            SOURCE_DIR,
        );
        request.resources[0]
            .attributes
            .insert(BROWSE_RECURSION_ATTRIBUTE.into(), recursion.into());
        request.options = rule_options(
            &request.tool,
            &request.subject,
            &request.executor,
            &request.resources,
            &request.input,
            &request.input_digest,
            Path::new("/project"),
            &PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            },
        );
        request
    }

    fn browse_rule(recursion: &str) -> StructuredPermissionRule {
        browse_request(recursion)
            .option_rule(
                if recursion == BROWSE_DIRECT {
                    BROWSE_OPTION_ID
                } else {
                    BROWSE_SUBTREE_OPTION_ID
                },
                PermissionLifetime::Conversation,
            )
            .unwrap()
    }

    #[test_case(BROWSE_DIRECT, BROWSE_DIRECT, SOURCE_DIR => true; "direct_same_root")]
    #[test_case(BROWSE_DIRECT, BROWSE_DIRECT, CHILD_DIR => false; "direct_not_descendants")]
    #[test_case(BROWSE_DIRECT, BROWSE_RECURSIVE, SOURCE_DIR => false; "direct_not_recursive")]
    #[test_case(BROWSE_RECURSIVE, BROWSE_DIRECT, SOURCE_DIR => true; "recursive_covers_direct")]
    #[test_case(BROWSE_RECURSIVE, BROWSE_DIRECT, CHILD_DIR => true; "recursive_covers_nested_listing")]
    #[test_case(BROWSE_RECURSIVE, BROWSE_RECURSIVE, CHILD_DIR => true; "recursive_covers_nested_glob")]
    #[test_case(BROWSE_RECURSIVE, BROWSE_RECURSIVE, SIBLING_DIR => false; "not_prefix_sibling")]
    #[test_case(BROWSE_RECURSIVE, BROWSE_RECURSIVE, "/project" => false; "not_parent")]
    #[test_case(BROWSE_RECURSIVE, "unknown", SOURCE_DIR => false; "unknown_recursion")]
    fn browse_scope_is_bounded(granted: &str, requested: &str, root: &str) -> bool {
        let rule = browse_rule(granted);
        let mut request = browse_request(requested);
        request.resources[0].value = root.into();
        permission_rule_covers_request(&rule, &request)
    }

    #[test_case(READ_CONTRACT, PermissionResourceKind::File, PermissionResourceAccess::Read; "content_read")]
    #[test_case(READ_CONTRACT, PermissionResourceKind::Directory, PermissionResourceAccess::Read; "unbound_directory_read")]
    #[test_case("file.grep.v1", PermissionResourceKind::Directory, PermissionResourceAccess::Search; "grep")]
    #[test_case("file.index.v1", PermissionResourceKind::Directory, PermissionResourceAccess::Search; "directory_index")]
    #[test_case("file.index.v1", PermissionResourceKind::File, PermissionResourceAccess::Read; "source_index")]
    #[test_case("code.map.v1", PermissionResourceKind::Directory, PermissionResourceAccess::Search; "code_graph")]
    #[test_case("file.write.v1", PermissionResourceKind::File, PermissionResourceAccess::Write; "write")]
    #[test_case("shell.execution.v1", PermissionResourceKind::Command, PermissionResourceAccess::Execute; "shell")]
    #[test_case(GLOB_CONTRACT, PermissionResourceKind::Directory, PermissionResourceAccess::Search; "generic_directory_search")]
    fn browse_never_covers_content_or_other_operations(
        contract: &str,
        kind: PermissionResourceKind,
        access: PermissionResourceAccess,
    ) {
        let rule = browse_rule(BROWSE_RECURSIVE);
        let request = workcell_request(contract, kind, access, SOURCE_DIR);
        assert!(!permission_rule_covers_request(&rule, &request));
    }

    #[test_case("file.grep.v1"; "grep_cannot_forge_list")]
    #[test_case("file.index.v1"; "index_cannot_forge_list")]
    #[test_case("file.write.v1"; "write_cannot_forge_list")]
    #[test_case("shell.execution.v1"; "shell_cannot_forge_list")]
    fn browse_checks_subject_even_when_it_is_identical(contract: &str) {
        let mut rule = browse_rule(BROWSE_RECURSIVE);
        rule.subject = PermissionSubject::Native {
            owner: "workcell".into(),
            contract: contract.into(),
        };
        let mut request = browse_request(BROWSE_RECURSIVE);
        request.subject = rule.subject.clone();
        assert!(!permission_rule_covers_request(&rule, &request));
    }

    #[test_case("lua"; "lua")]
    #[test_case("mcp"; "mcp")]
    #[test_case("remote"; "remote_workcell")]
    #[test_case("remote_native"; "remote_native")]
    #[test_case("other_owner"; "native_other_owner")]
    fn browse_does_not_cross_trust_domains(domain: &str) {
        let mut request = browse_request(BROWSE_RECURSIVE);
        request.subject = match domain {
            "lua" => PermissionSubject::Lua {
                plugin: "workcell".into(),
                tool: "file_glob".into(),
                contract: GLOB_CONTRACT.into(),
            },
            "mcp" => PermissionSubject::Mcp {
                server: "workcell".into(),
                authority: "workcell".into(),
                tool: "file_glob".into(),
                contract: GLOB_CONTRACT.into(),
            },
            "remote" => PermissionSubject::RemoteWorkcell {
                identity: default_remote_identity(),
                tool: "file_glob".into(),
                contract: GLOB_CONTRACT.into(),
            },
            "remote_native" => PermissionSubject::RemoteNative {
                identity: default_remote_identity(),
                owner: "workcell".into(),
                contract: GLOB_CONTRACT.into(),
            },
            _ => PermissionSubject::Native {
                owner: "other".into(),
                contract: GLOB_CONTRACT.into(),
            },
        };
        let mut rule = browse_rule(BROWSE_RECURSIVE);
        assert!(!permission_rule_covers_request(&rule, &request));
        rule.subject = request.subject.clone();
        assert!(!permission_rule_covers_request(&rule, &request));
    }

    #[test_case(BROWSE_DIRECT; "direct")]
    #[test_case(BROWSE_RECURSIVE; "recursive")]
    fn content_read_grants_cover_browse_but_list_constraints_never_grant_content(recursion: &str) {
        let request = browse_request(recursion);
        assert!(permission_rule_covers_request(
            &read_subtree_rule(SUBTREE_OPTION),
            &request
        ));
        let mut rule = browse_rule(recursion);
        rule.family = Some(PermissionCapabilityFamily::FilesystemRead);
        let content = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_DIR,
        );
        assert!(!permission_rule_covers_request(&rule, &content));
        assert!(PermissionRuleRecord::conversation(rule).is_err());
    }

    #[test_case(BROWSE_DIRECT, 2; "direct_and_explicit_recursive")]
    #[test_case(BROWSE_RECURSIVE, 1; "glob_only_recursive")]
    fn browse_options_never_climb_ancestors(recursion: &str, count: usize) {
        let request = browse_request(recursion);
        let options: Vec<_> = request
            .options
            .iter()
            .filter(|option| option.rule.family.is_some())
            .collect();
        assert_eq!(options.len(), count);
        assert!(
            options
                .iter()
                .all(|option| option.rule.family
                    == Some(PermissionCapabilityFamily::FilesystemBrowse))
        );
        for option in options {
            let direct = option.id == BROWSE_OPTION_ID;
            assert_eq!(
                option.label,
                if direct {
                    EXACT_FOLDER_LABEL
                } else {
                    BROWSE_NAMES_LABEL
                }
            );
            assert_eq!(option.is_default, direct || recursion == BROWSE_RECURSIVE);
            assert!(permission_rule_covers_request(&option.rule, &request));
            let stored = request
                .option_rule(&option.id, PermissionLifetime::Conversation)
                .unwrap();
            assert!(PermissionRuleRecord::conversation(stored).is_ok());
            let review = review_for_rule(&request, &option.rule);
            assert_eq!(
                review.resources[0].attributes[BROWSE_RECURSION_ATTRIBUTE],
                if direct {
                    BROWSE_DIRECT
                } else {
                    BROWSE_RECURSIVE
                }
            );
            let mut parent = browse_request(recursion);
            parent.resources[0].value = "/project".into();
            assert!(!permission_rule_covers_request(&option.rule, &parent));
        }
        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option.is_default)
                .count(),
            1
        );
    }

    #[test_case("missing_recursion"; "missing_recursion")]
    #[test_case("protected"; "protected_descendant")]
    #[test_case("unbounded"; "unbounded_root")]
    #[test_case("direct_subtree"; "direct_subtree")]
    #[test_case("executor"; "nonnative_executor")]
    fn malformed_browse_grants_fail_closed(change: &str) {
        let mut rule = browse_rule(BROWSE_RECURSIVE);
        let mut request = browse_request(BROWSE_RECURSIVE);
        match change {
            "missing_recursion" => {
                request.resources[0].attributes.clear();
            }
            "protected" => {
                request.resources[0].protected = true;
                request.resources[0].value = PROTECTED_DIR.into();
            }
            "unbounded" => rule.resources[0].selector = PermissionResourceSelector::Any,
            "direct_subtree" => {
                rule.resources[0].attributes =
                    browse_rule(BROWSE_DIRECT).resources[0].attributes.clone()
            }
            _ => {
                rule.executor = PermissionExecutorKind::Mcp;
                request.executor = PermissionExecutorKind::Mcp;
            }
        }
        assert!(!permission_rule_covers_request(&rule, &request));
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::{COMMAND_PATTERNS_LABEL, EXACT_COMMAND_LABEL, EXACT_COMMANDS_LABEL};
    use crate::tools::PermissionScopes;
    use serde_json::json;

    use test_case::test_case;

    use crate::permissions::structured::tests::{
        EXPECT_SUBTREE_OPTION, PROJECT_ROOT_MARK, PROJECT_RUNG, PROTECTED_PATH, READ_CONTRACT,
        SOURCE_FILE, SUBTREE_OPTION, caution_of, command_resource, default_remote_identity,
        explicit_request, ladder_values, protected_command_resource, read_subtree_rule,
        remote_request_resource, subtree_ladder, url_ladder, webfetch_request, workcell_request,
    };
    use crate::permissions::structured::{
        EXACT_COMMAND_CHIP, GIT_METADATA_DIR, MAX_URL_LADDER_RUNGS, NATIVE_OWNER,
        OUTSIDE_HOME_PHRASE, PermissionArgumentConstraint, PermissionAuthorityProfile,
        PermissionCapabilityFamily, PermissionCaution, PermissionExecutorKind, PermissionIntent,
        PermissionLifetime, PermissionRequest, PermissionResource, PermissionResourceAccess,
        PermissionResourceKind, PermissionResourceSelector, PermissionRisk, PermissionSubject,
        StructuredPermissionEffect, URL_ORIGIN_OPTION_ID, URL_SUBTREE_OPTION_ID, WORKCELL_OWNER,
        canonical_json, exact_resource_constraints, permission_rule_covers_request,
        permission_rule_covers_resource,
    };
    use caudra_config::ToolKey;
    use std::collections::BTreeMap;
    use std::path::Path;
    #[test]
    fn remote_directory_grant_uses_complete_opaque_ancestry() {
        let identity = default_remote_identity();
        let request = remote_request_resource(
            identity.clone(),
            PermissionResourceKind::RemoteDirectory {
                identity: identity.clone(),
            },
            "root\u{1f}parent\u{1f}directory",
            false,
        );
        let rule = request
            .options
            .iter()
            .find(|option| option.id == "allow_remote_resources")
            .unwrap()
            .rule
            .clone();
        let mut descendant = request.clone();
        descendant.resources[0].kind = PermissionResourceKind::RemoteFile { identity };
        descendant.resources[0].value = "root\u{1f}parent\u{1f}directory\u{1f}file".into();
        assert!(permission_rule_covers_request(&rule, &descendant));

        descendant.resources[0].value = "root\u{1f}cloned-directory\u{1f}file".into();
        assert!(!permission_rule_covers_request(&rule, &descendant));
    }

    #[test]
    fn mcp_options_keep_broad_choice_non_default() {
        let request = PermissionRequest::from_legacy(
            "mcp".into(),
            ToolKey::McpTool {
                server: Arc::from("server"),
                tool: Arc::from("lookup"),
            },
            vec![canonical_json(&json!({"query": "value"}))],
            json!({"query": "value"}),
            Path::new("/tmp"),
            false,
        );
        let broad = request.options.iter().find(|option| option.broad).unwrap();
        assert!(!broad.is_default);
        assert_eq!(broad.allowed_lifetimes, [PermissionLifetime::Conversation]);
        assert!(matches!(
            broad.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ));
        assert!(
            request
                .options
                .iter()
                .filter(|option| option.rule.effect == StructuredPermissionEffect::Allow)
                .filter(|option| !option.broad)
                .all(|option| matches!(
                    option.rule.arguments,
                    PermissionArgumentConstraint::Exact { .. }
                ))
        );
        for lifetime in [
            PermissionLifetime::Once,
            PermissionLifetime::Conversation,
            PermissionLifetime::Project,
            PermissionLifetime::Global,
        ] {
            assert!(request.options.iter().any(|option| {
                option.rule.effect == StructuredPermissionEffect::Allow
                    && !option.broad
                    && option.allowed_lifetimes.contains(&lifetime)
            }));
        }
        let serialized = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<PermissionRequest>(&serialized).unwrap(),
            request
        );
    }

    #[test]
    fn a_url_ladder_climbs_one_path_segment_at_a_time() {
        assert_eq!(
            url_ladder(&webfetch_request("https://example.com/path/to/sub/page")),
            [
                (
                    "allow_url_subtree",
                    "https://example.com/path/to/sub/page/**"
                ),
                ("allow_url_subtree_1", "https://example.com/path/to/sub/**"),
                ("allow_url_subtree_2", "https://example.com/path/to/**"),
                ("allow_url_subtree_3", "https://example.com/path/**"),
                ("allow_url_origin", "https://example.com/**"),
            ]
        );
    }

    /// A rung is worth walking to only if it reaches further than the one below
    /// and no further than the one above.
    #[test]
    fn each_url_rung_reaches_its_own_prefix_and_no_further() {
        let request = webfetch_request("https://example.com/path/to/sub/page");
        let reaches = |rung: &str, url: &str| {
            let rule = request
                .option_rule(rung, PermissionLifetime::Conversation)
                .expect(EXPECT_SUBTREE_OPTION);
            permission_rule_covers_request(&rule, &webfetch_request(url))
        };
        assert!(reaches(
            "allow_url_subtree_2",
            "https://example.com/path/to/other"
        ));
        assert!(!reaches(
            "allow_url_subtree_2",
            "https://example.com/path/other"
        ));
        assert!(reaches(
            "allow_url_subtree_3",
            "https://example.com/path/other"
        ));
        assert!(!reaches(
            "allow_url_subtree_3",
            "https://example.com/elsewhere"
        ));
        assert!(reaches("allow_url_origin", "https://example.com/elsewhere"));
        assert!(!reaches("allow_url_origin", "https://other.example/path"));
    }

    /// The cap bounds the ladder from the origin end, because the page the
    /// request named is the rung that has to be there.
    #[test]
    fn a_long_url_path_offers_its_deepest_rungs_and_the_origin() {
        let path = (1..=12).map(|n| format!("s{n}")).collect::<Vec<_>>();
        let request = webfetch_request(&format!("https://example.com/{}", path.join("/")));
        let rungs = url_ladder(&request);
        assert_eq!(rungs.len(), MAX_URL_LADDER_RUNGS + 1);
        assert_eq!(
            rungs[0].1,
            format!("https://example.com/{}/**", path.join("/"))
        );
        assert_eq!(rungs[MAX_URL_LADDER_RUNGS].0, URL_ORIGIN_OPTION_ID);
    }

    #[test]
    fn webfetch_options_cover_exact_url_subtree_origin_and_any_url() {
        let request = PermissionRequest::from_legacy(
            "webfetch".into(),
            ToolKey::native("webfetch"),
            vec!["http://example.com/docs/page?token=secret#fragment".into()],
            json!({
                "url": "http://example.com/docs/page?token=secret#fragment",
                "format": "markdown"
            }),
            Path::new("/project"),
            false,
        );
        for option in [
            "allow_exact",
            "allow_exact_url",
            "allow_url_subtree",
            "allow_url_origin",
            "allow_any_url",
        ] {
            assert!(
                request
                    .options
                    .iter()
                    .any(|candidate| candidate.id == option),
                "missing {option}"
            );
        }
        assert_eq!(
            request.resources[0].value,
            "https://example.com/docs/page?token=secret"
        );
        assert!(!request.presentation.resources[0].summary.contains("secret"));

        let descendant = PermissionRequest::from_legacy(
            "descendant".into(),
            ToolKey::native("webfetch"),
            vec!["https://example.com/docs/page/child?other=value".into()],
            json!({"url": "https://example.com/docs/page/child?other=value", "timeout": 10}),
            Path::new("/project"),
            false,
        );
        let sibling = PermissionRequest::from_legacy(
            "sibling".into(),
            ToolKey::native("webfetch"),
            vec!["https://example.com/docs/other".into()],
            json!({"url": "https://example.com/docs/other"}),
            Path::new("/project"),
            false,
        );
        let other_origin = PermissionRequest::from_legacy(
            "other".into(),
            ToolKey::native("webfetch"),
            vec!["https://other.example/path".into()],
            json!({"url": "https://other.example/path"}),
            Path::new("/project"),
            false,
        );

        let subtree = request
            .option_rule("allow_url_subtree", PermissionLifetime::Conversation)
            .unwrap();
        assert!(permission_rule_covers_request(&subtree, &descendant));
        assert!(!permission_rule_covers_request(&subtree, &sibling));
        let origin = request
            .option_rule("allow_url_origin", PermissionLifetime::Project)
            .unwrap();
        assert!(permission_rule_covers_request(&origin, &sibling));
        assert!(!permission_rule_covers_request(&origin, &other_origin));
        let any = request
            .option_rule("allow_any_url", PermissionLifetime::Global)
            .unwrap();
        assert!(permission_rule_covers_request(&any, &other_origin));

        let persisted = serde_json::to_string(&any).unwrap();
        assert!(!persisted.contains("example.com"));
        assert!(!persisted.contains("secret"));
    }

    #[test]
    fn shell_pattern_option_preserves_exact_fallbacks_and_resource_context() {
        let resources = vec![
            command_resource("git diff --stat", "/project"),
            command_resource("git status --short", "/project"),
            command_resource(r#"printf "%s\n" done"#, "/project"),
        ];
        let exact_constraints = exact_resource_constraints(&resources);
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            resources,
            json!({"command": "multiple", "timeoutSec": 30}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(option.label, COMMAND_PATTERNS_LABEL);
        assert_eq!(
            option.description,
            r#"Allow commands matching these patterns in /project: git diff *, git status *. Also allow `printf "%s\n" done` exactly as reviewed."#
        );
        assert!(option.broad);
        assert!(!option.is_default);
        assert_eq!(option.confirmation, None);
        assert_eq!(
            option.allowed_lifetimes,
            [
                PermissionLifetime::Conversation,
                PermissionLifetime::Project,
                PermissionLifetime::Global,
            ]
        );
        assert!(matches!(
            option.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ));
        assert!(matches!(
            &option.rule.resources[0].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "git diff *"
        ));
        assert!(matches!(
            &option.rule.resources[1].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "git status *"
        ));
        assert_eq!(
            option.rule.resources[2].selector,
            exact_constraints[2].selector
        );
        for (constraint, exact) in option.rule.resources.iter().zip(&exact_constraints) {
            assert_eq!(constraint.kind, exact.kind);
            assert_eq!(constraint.access, exact.access);
            assert_eq!(constraint.protected, exact.protected);
            assert_eq!(constraint.attributes, exact.attributes);
        }
    }

    #[test]
    fn shell_pattern_option_uses_a_single_prefix_label() {
        let mut resource = command_resource("/usr/bin/git diff --stat", "/project");
        resource.attributes.insert(
            crate::permissions::NORMALIZED_COMMAND_ATTRIBUTE.into(),
            "git diff --stat".into(),
        );
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![resource],
            json!({"command": "/usr/bin/git diff --stat"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(option.label, "/usr/bin/git diff *");
        assert!(
            !option.rule.resources[0]
                .attributes
                .contains_key(crate::permissions::NORMALIZED_COMMAND_ATTRIBUTE)
        );

        let mut next_resource = command_resource("/usr/bin/git diff --check", "/project");
        next_resource.attributes.insert(
            crate::permissions::NORMALIZED_COMMAND_ATTRIBUTE.into(),
            "git diff --check".into(),
        );
        let next = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![next_resource],
            json!({"command": "/usr/bin/git diff --check"}),
        );
        assert!(permission_rule_covers_request(&option.rule, &next));
    }

    #[test]
    fn shell_pattern_option_reuses_a_reviewed_subcommand() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource(r#"git commit -m "first""#, "/project")],
            json!({"command": r#"git commit -m "first""#}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(option.label, "git commit *");

        let next = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource(r#"git commit -m "second""#, "/project")],
            json!({"command": r#"git commit -m "second""#}),
        );
        assert!(permission_rule_covers_request(&option.rule, &next));
    }

    #[test]
    fn shell_pattern_option_keeps_a_curated_search_term_out_of_the_rule() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("rg needle src/", "/project")],
            json!({"command": "rg needle src/"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(option.label, "rg *");

        let next = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("rg other tests/", "/project")],
            json!({"command": "rg other tests/"}),
        );
        assert!(permission_rule_covers_request(&option.rule, &next));
    }

    #[test]
    fn shell_pattern_option_is_absent_for_builtin_ask_families() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("git checkout main --force", "/project")],
            json!({"command": "git checkout main --force"}),
        );

        assert!(
            request
                .options
                .iter()
                .all(|option| option.id != "allow_command_patterns")
        );
    }

    #[test]
    fn shell_pattern_option_is_absent_without_a_reusable_prefix() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![
                command_resource("rm -rf /tmp/build", "/project"),
                command_resource(r#"printf "%s\n" done"#, "/project"),
            ],
            json!({"command": "multiple"}),
        );

        assert!(
            request
                .options
                .iter()
                .all(|option| option.id != "allow_command_patterns")
        );
    }

    #[test]
    fn exact_command_option_names_every_reviewed_command_and_workdir() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![
                command_resource("cargo test", "/project"),
                command_resource("git status --short", "/project"),
            ],
            json!({"command": "multiple"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_exact_commands")
            .unwrap();

        assert_eq!(option.label, EXACT_COMMANDS_LABEL);
        assert_eq!(
            option.description,
            "Allow `cargo test`, `git status --short` in /project with different timeout or display controls."
        );
    }

    #[test]
    fn exact_command_option_uses_a_singular_label_for_one_command() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("cargo test", "/project")],
            json!({"command": "cargo test"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_exact_commands")
            .unwrap();

        assert_eq!(option.label, EXACT_COMMAND_LABEL);
        assert_eq!(
            option.description,
            "Allow `cargo test` in /project with different timeout or display controls."
        );
    }

    /// A rung's label is the scope of a sentence, `allow ‹label› for this
    /// conversation`, naming the workdir only when it is not the project.
    #[test_case("/project", &["this exact command", "cargo test *", "cargo *"]; "project_root")]
    #[test_case("/project/crates/core", &["this exact command in crates/core/", "cargo test * in crates/core/", "cargo * in crates/core/"]; "inside_the_project")]
    #[test_case("/elsewhere", &["this exact command in /elsewhere/", "cargo test * in /elsewhere/", "cargo * in /elsewhere/"]; "outside_the_project")]
    fn command_rungs_read_as_scopes(workdir: &str, labels: &[&str]) {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("cargo test -p core", workdir)],
            json!({"command": "cargo test -p core"}),
        );

        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option.group.as_ref().and_then(|group| group.resource) == Some(0))
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            labels
        );
    }

    #[test]
    fn web_fetch_defaults_to_this_page_and_below() {
        let request =
            webfetch_request("https://docs.rs/ratatui/latest/widgets/struct.Paragraph.html");

        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option
                    .group
                    .as_ref()
                    .is_some_and(|group| group.key == URL_SUBTREE_OPTION_ID))
                .map(|option| (option.label.as_str(), option.is_default))
                .collect::<Vec<_>>(),
            [
                ("this page and below", true),
                ("pages under docs.rs/ratatui/latest/widgets/", false),
                ("pages under docs.rs/ratatui/latest/", false),
                ("pages under docs.rs/ratatui/", false),
                ("any page on docs.rs", false),
            ]
        );
    }

    #[test]
    fn file_rungs_read_as_scopes_from_the_file_outward() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            "/project/src/lib.rs",
        );

        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option
                    .group
                    .as_ref()
                    .is_some_and(|group| group.key == SUBTREE_OPTION))
                .map(|option| (option.label.as_str(), option.is_default))
                .collect::<Vec<_>>(),
            [
                ("this file", true),
                ("src/", false),
                ("this project", false),
                ("anything under /", false),
            ]
        );
    }

    /// Choices 2 and 3 start on one rung per ladder, so the exact call is the
    /// default only where the request offers no ladder at all.
    #[test_case(command_resource("cargo test -p core", "/project"), "command_pattern_0"; "a_command_with_a_prefix")]
    #[test_case(command_resource("cargo test", "/project"), "command_exact_0"; "a_command_without_a_prefix")]
    #[test_case(protected_command_resource("cargo test -p core", "/project"), "allow_exact"; "a_protected_line")]
    fn each_request_starts_on_one_default(resource: PermissionResource, default: &str) {
        let command = resource.value.clone();
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![resource],
            json!({ "command": command }),
        );

        assert_eq!(
            request
                .options
                .iter()
                .filter(|option| option.is_default)
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            [default]
        );
    }

    #[test]
    fn each_command_earns_a_ladder_that_speaks_only_for_itself() {
        let resources = vec![
            command_resource("git status --short", "/project"),
            command_resource(r#"printf "%s\n" done"#, "/project"),
        ];
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            resources.clone(),
            json!({"command": "multiple"}),
        );
        let rungs = |index: usize| {
            request
                .options
                .iter()
                .filter(|option| {
                    option.group.as_ref().and_then(|group| group.resource) == Some(index)
                })
                .collect::<Vec<_>>()
        };
        let chips = |index: usize| {
            rungs(index)
                .into_iter()
                .map(|option| {
                    (
                        option.id.clone(),
                        option.group.as_ref().unwrap().value.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            chips(0),
            [
                ("command_exact_0".to_owned(), EXACT_COMMAND_CHIP.to_owned()),
                ("command_pattern_0".to_owned(), "git status *".to_owned()),
            ]
        );
        assert_eq!(
            chips(1),
            [
                ("command_exact_1".to_owned(), EXACT_COMMAND_CHIP.to_owned()),
                ("command_prefix_1_1".to_owned(), "printf *".to_owned()),
            ]
        );
        for index in 0..resources.len() {
            for option in rungs(index) {
                assert_eq!(
                    option.group.as_ref().unwrap().key,
                    format!("command_{index}")
                );
                assert_eq!(option.rule.resources.len(), 1);
                assert!(permission_rule_covers_resource(
                    &option.rule,
                    &request,
                    &resources[index]
                ));
                assert!(!permission_rule_covers_resource(
                    &option.rule,
                    &request,
                    &resources[1 - index]
                ));
            }
        }
    }

    /// Each rung reads as the scope it grants, whether it is where the row
    /// starts, and whether it needs confirming. A broad rung is confirmed as
    /// broad shell access and never claims the filesystem reach of a folder.
    #[test_case("rustfmt --edition 2024 --check f.rs", &[
        ("command_exact_0", EXACT_COMMAND_CHIP, true, false),
        ("command_prefix_0_4", "rustfmt --edition 2024 --check *", false, false),
        ("command_prefix_0_3", "rustfmt --edition 2024 *", false, false),
        ("command_prefix_0_1", "rustfmt *", false, true),
    ]; "a_command_without_a_prefix_starts_exact")]
    #[test_case("cargo test -p core", &[
        ("command_exact_0", EXACT_COMMAND_CHIP, false, false),
        ("command_pattern_0", "cargo test *", true, false),
        ("command_prefix_0_1", "cargo *", false, true),
    ]; "the_reusable_prefix_is_one_of_the_ancestors")]
    #[test_case("python3 x.py", &[
        ("command_exact_0", EXACT_COMMAND_CHIP, true, false),
    ]; "an_interpreter_never_reaches_its_bare_rung")]
    fn a_row_climbs_its_ancestors_to_a_broad_bare_rung(
        command: &str,
        expected: &[(&str, &str, bool, bool)],
    ) {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource(command, "/project")],
            json!({ "command": command }),
        );

        let ladder: Vec<_> = request
            .options
            .iter()
            .filter(|option| option.group.as_ref().and_then(|group| group.resource) == Some(0))
            .map(|option| {
                (
                    option.id.as_str(),
                    option.group.as_ref().unwrap().value.as_str(),
                    option.is_default,
                    option.confirmation.is_some(),
                )
            })
            .collect();
        assert_eq!(ladder, expected);
        let cautions: Vec<_> = request
            .options
            .iter()
            .filter_map(|option| option.caution.map(|caution| (&option.id, caution)))
            .collect();
        assert!(cautions.is_empty(), "{cautions:?}");
    }

    /// A file two levels inside the project can be widened all the way out, so
    /// reading a sibling checkout no longer stops at the file's own directory.
    #[test]
    fn the_subtree_ladder_climbs_to_the_filesystem_root() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );

        assert_eq!(
            ladder_values(&request),
            vec![
                "/project/src/**".to_string(),
                format!("/project/** {PROJECT_ROOT_MARK}"),
                "/**".to_string(),
            ]
        );
        assert_eq!(
            subtree_ladder(&request)
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec![SUBTREE_OPTION, PROJECT_RUNG, "allow_filesystem_subtree_2"]
        );
    }

    /// The widest rung still has to grant what the request asked for, or the
    /// ladder is a row of labels that authorise nothing.
    #[test]
    fn the_widest_rung_still_covers_the_requested_path() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        let widest = subtree_ladder(&request)
            .last()
            .expect("the ladder has a widest rung")
            .id
            .clone();
        let rule = request
            .option_rule(&widest, PermissionLifetime::Conversation)
            .expect("the widest rung is grantable");

        assert!(permission_rule_covers_request(&rule, &request));
    }

    /// The ladder may reach the filesystem root, so this is what keeps a
    /// credential out of reach however far it is widened. Two guards say so
    /// independently — every rung pins `protected: Some(false)`, and a subtree
    /// selector is not exact enough to grant a protected resource — so the
    /// outcome is asserted rather than either mechanism.
    #[test]
    fn no_rung_can_reach_a_protected_path() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        let intent = PermissionIntent::new(
            PermissionScopes::single(PROTECTED_PATH.into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::File,
                value: PROTECTED_PATH.into(),
                access: Some(PermissionResourceAccess::Read),
                protected: true,
                requires_prompt: true,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Medium,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        });
        let secret = PermissionRequest::from_intent_with_identity(
            "request".into(),
            ToolKey::native("workcell_file_tool"),
            &intent,
            json!({ "path": PROTECTED_PATH }),
            Path::new("/project"),
            PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: READ_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );

        for rung in subtree_ladder(&request) {
            let rule = request
                .option_rule(&rung.id, PermissionLifetime::Conversation)
                .expect("a rung is grantable");
            assert!(
                !permission_rule_covers_request(&rule, &secret),
                "{} reached {PROTECTED_PATH}",
                rung.id
            );
        }
    }

    /// Widening is uncautioned inside the repository, warned once it reaches
    /// past it, and grave once the grant would swallow the home directory.
    #[test]
    fn the_ladder_cautions_each_rung_by_what_it_reaches() {
        let Some(home) = caudra_storage::paths::home() else {
            return;
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let repository = temp.path().join("repo");
        std::fs::create_dir_all(repository.join(GIT_METADATA_DIR)).expect("git marker");
        std::fs::create_dir_all(repository.join("src")).expect("source dir");

        // A repository outside home: its own rungs are clean, the rungs above
        // it warn, and only the root — which contains home — is grave. Sitting
        // outside home is not the same as reaching over it.
        let cautions = caution_of(&repository, &repository.join("src/main.rs"));
        assert_eq!(cautions.first(), Some(&None));
        assert!(
            cautions.contains(&Some(PermissionCaution::Warn)),
            "{cautions:?}"
        );
        assert_eq!(cautions.last(), Some(&Some(PermissionCaution::Danger)));
        assert_eq!(
            cautions
                .iter()
                .filter(|caution| **caution == Some(PermissionCaution::Danger))
                .count(),
            1,
            "{cautions:?}"
        );

        // A path under home with no repository at all warns from the start and
        // still ends grave at the root.
        let cautions = caution_of(&home, &home.join("notes/todo.md"));
        assert_eq!(cautions.first(), Some(&Some(PermissionCaution::Warn)));
        assert_eq!(cautions.last(), Some(&Some(PermissionCaution::Danger)));
    }

    /// A grant that swallows home has to be typed out, whatever else it does.
    #[test]
    fn a_rung_outside_home_demands_the_typed_phrase() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        let ladder = subtree_ladder(&request);
        let widest = ladder.last().expect("the ladder has a widest rung");

        assert_eq!(widest.caution, Some(PermissionCaution::Danger));
        assert_eq!(widest.confirmation.as_deref(), Some(OUTSIDE_HOME_PHRASE));
    }

    /// The subject check already rejects a real write, since the write contracts
    /// are not family members. This pins the independent resource check, so a
    /// caller presenting a reading contract with a write resource is still
    /// refused rather than relying on subject filtering alone.
    #[test]
    fn a_read_family_grant_refuses_a_write_resource_from_a_reading_contract() {
        let rule = read_subtree_rule(SUBTREE_OPTION);
        let mut forged = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        forged.resources[0].access = Some(PermissionResourceAccess::Write);

        assert_eq!(rule.subject, forged.subject);
        assert!(!permission_rule_covers_request(&rule, &forged));
    }

    #[test]
    fn a_read_family_grant_stays_inside_its_trust_domain() {
        let rule = read_subtree_rule(SUBTREE_OPTION);
        let mut outsider = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        outsider.subject = PermissionSubject::Native {
            owner: NATIVE_OWNER.into(),
            contract: READ_CONTRACT.into(),
        };

        assert!(!permission_rule_covers_request(&rule, &outsider));
    }

    #[test_case(SUBTREE_OPTION; "subtree grant is widened")]
    #[test_case(PROJECT_RUNG; "project grant is widened")]
    fn a_first_party_read_mints_the_filesystem_read_family(option: &str) {
        assert_eq!(
            read_subtree_rule(option).family,
            Some(PermissionCapabilityFamily::FilesystemRead)
        );
    }
}
