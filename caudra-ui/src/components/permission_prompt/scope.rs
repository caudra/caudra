use std::path::Path;

use caudra_agent::permissions::{
    PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionExecutorKind,
    PermissionLifetime, PermissionRequest, PermissionResourceAccess, PermissionResourceConstraint,
    PermissionResourceKind, PermissionResourceSelector, PermissionReview, PermissionReviewResource,
    PermissionRuleOption, PermissionSubject, StructuredPermissionRule,
    review::{command_template_phrase, recovered_value, review_for_rule},
};
use caudra_storage::permission_patterns::PatternDefinition;

use super::details::{INCOMPLETE_REDACTION, TRUNCATED, redact_url_query, review_text};
use super::inspector::{offered_pattern, pattern_summary};
use super::notes::tilde;
use crate::components::permission_scope::model::{ScopeActivity, ScopeModel, ScopeSource};

pub(super) const SHELL_REACH: &str =
    "Commands may read or change files elsewhere. The starting folder is not a sandbox.";
pub(crate) const MISSING_SCOPE: &str = "Some details of this scope can't be shown.";
const PROTECTED_SCOPE: &str = "Includes protected files such as keys and settings.";
const EXACT_CALL: &str = "Only this exact call.";
const SELECTED_INPUTS: &str = "Some inputs stay fixed; the others may vary.";
const ANY_ARGUMENTS: &str = "Any arguments.";
const POSSIBLE_WORKDIRS_ATTRIBUTE: &str = "possible_workdirs";
const WORKDIR_ATTRIBUTE: &str = "workdir";
const UNAVAILABLE: &str = "unavailable";
const OMITTED_MARKER: &str = "[omitted:";
const WILDCARD_SUFFIX: &str = " *";
const HTTPS_SCHEME: &str = "https://";
const FOLDER_SEPARATOR: char = '/';
const HIDDEN: &str = "Caudra can't show";
const THIS_PROJECT: &str = " (this project)";
const MAX_ROW_TARGETS: usize = 2;

/// What one rung allows, as plain lines, and whether every part of it could
/// be shown. A rung that can't be shown whole is never offered on a keypress.
pub(crate) struct ScopeSummary {
    pub lines: Vec<String>,
    pub complete: bool,
}

/// How far one resource constraint reaches, with the value the review
/// recovered for it. `Hidden` is a pinned value the review couldn't recover.
enum Reach {
    Exact(String),
    Under(String),
    Origin(String),
    Prefix(String),
    Pattern(String),
    Template(String),
    Any,
    Remote,
    RemoteUnder,
    Hidden,
}

/// Where a command may start.
enum Start {
    Anywhere,
    In(String),
    Hidden,
}

fn exact_selector(selector: &PermissionResourceSelector) -> bool {
    matches!(
        selector,
        PermissionResourceSelector::Exact { .. }
            | PermissionResourceSelector::Digest { .. }
            | PermissionResourceSelector::RemoteResource { .. }
    )
}

fn shown(review: Option<&PermissionReview>, index: usize) -> Option<&PermissionReviewResource> {
    review?
        .resources
        .iter()
        .find(|resource| resource.index == index)
}

fn reach(
    resource: &PermissionResourceConstraint,
    shown: Option<&PermissionReviewResource>,
) -> Reach {
    let value = shown
        .and_then(|shown| shown.value.as_deref())
        .and_then(|label| recovered_value(&resource.selector, label))
        .map(str::to_owned);
    let known = |reach: fn(String) -> Reach| value.clone().map_or(Reach::Hidden, reach);
    match &resource.selector {
        PermissionResourceSelector::Any => Reach::Any,
        PermissionResourceSelector::CommandTemplate { definition } => {
            Reach::Template(command_template_phrase(definition))
        }
        PermissionResourceSelector::RemoteResource { .. } => Reach::Remote,
        PermissionResourceSelector::RemoteSubtree { .. } => Reach::RemoteUnder,
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. } => {
            known(Reach::Exact)
        }
        PermissionResourceSelector::Subtree { .. }
        | PermissionResourceSelector::FilesystemSubtreeDigest { .. }
        | PermissionResourceSelector::UrlSubtreeDigest { .. } => known(Reach::Under),
        PermissionResourceSelector::UrlOriginDigest { .. } => known(Reach::Origin),
        PermissionResourceSelector::Prefix { .. } => known(Reach::Prefix),
        PermissionResourceSelector::CommandPattern { .. } => known(Reach::Pattern),
    }
}

fn start(
    resource: &PermissionResourceConstraint,
    shown: Option<&PermissionReviewResource>,
) -> Start {
    let Some(selector) = resource.attributes.get(WORKDIR_ATTRIBUTE) else {
        return Start::Anywhere;
    };
    shown
        .and_then(|shown| shown.attributes.get(WORKDIR_ATTRIBUTE))
        .and_then(|label| recovered_value(selector, label))
        .map_or(Start::Hidden, |workdir| Start::In(workdir.to_owned()))
}

/// The verb a sentence and a row name an access by.
fn verbs(access: Option<&PermissionResourceAccess>) -> (&'static str, &'static str) {
    match access {
        Some(PermissionResourceAccess::Read) => ("reading", "Reads"),
        Some(PermissionResourceAccess::List) => ("listing", "Lists"),
        Some(PermissionResourceAccess::Write) => ("editing", "Changes"),
        Some(PermissionResourceAccess::Execute) => ("running", "Runs"),
        Some(PermissionResourceAccess::Search) => ("searching", "Searches"),
        Some(PermissionResourceAccess::Connect) => ("connecting to", "Connects to"),
        None => ("using", "Uses"),
    }
}

/// A folder the way a scope names it: `~`-abbreviated, ending in a slash.
fn folder(path: &str) -> String {
    let place = tilde(path);
    match place.ends_with(FOLDER_SEPARATOR) {
        true => place,
        false => format!("{place}{FOLDER_SEPARATOR}"),
    }
}

/// A URL the way a scope names it: the scheme only when it is not https.
fn url_place(url: &str) -> String {
    review_text(url.strip_prefix(HTTPS_SCHEME).unwrap_or(url))
}

fn pattern_words(pattern: &str) -> String {
    match pattern.strip_suffix(WILDCARD_SUFFIX) {
        Some(prefix) => format!("`{}` with any arguments", review_text(prefix)),
        None => format!("exactly `{}`", review_text(pattern)),
    }
}

/// The object of a sentence about one resource: what it reaches.
fn reach_object(resource: &PermissionResourceConstraint, reach: &Reach) -> String {
    match (&resource.kind, reach) {
        (_, Reach::Remote) => "this remote resource".into(),
        (_, Reach::RemoteUnder) => "this remote resource and everything under it".into(),
        (PermissionResourceKind::Command, Reach::Exact(command)) => {
            format!("exactly `{}`", review_text(command))
        }
        (PermissionResourceKind::Command, Reach::Pattern(pattern)) => pattern_words(pattern),
        (PermissionResourceKind::Command, Reach::Template(template)) => {
            format!("`{}`", review_text(template))
        }
        (PermissionResourceKind::Command, Reach::Prefix(prefix)) => {
            format!("commands starting with `{}`", review_text(prefix))
        }
        (PermissionResourceKind::Command, Reach::Any) => "any command".into(),
        (PermissionResourceKind::Command, Reach::Hidden) => format!("one command {HIDDEN}"),
        (PermissionResourceKind::Url, Reach::Exact(url)) => review_text(&redact_url_query(url)),
        (PermissionResourceKind::Url, Reach::Under(root)) => {
            format!("pages under {}/", url_place(root))
        }
        (PermissionResourceKind::Url, Reach::Origin(origin)) => {
            format!("any page on {}", url_place(origin))
        }
        (PermissionResourceKind::Url, Reach::Any) => "any public web page".into(),
        (PermissionResourceKind::Url, Reach::Hidden) => format!("one page {HIDDEN}"),
        (PermissionResourceKind::Query, Reach::Exact(query)) => {
            format!("the search “{}”", review_text(query))
        }
        (PermissionResourceKind::Query, Reach::Any) => "any search".into(),
        (PermissionResourceKind::Query, Reach::Hidden) => format!("one search {HIDDEN}"),
        (_, Reach::Exact(path)) => tilde(path),
        (_, Reach::Under(root)) => format!("anything under {}", folder(root)),
        (_, Reach::Prefix(prefix)) | (_, Reach::Pattern(prefix)) => {
            format!("anything starting with {}", review_text(prefix))
        }
        (_, Reach::Origin(origin)) => review_text(origin),
        (_, Reach::Template(template)) => review_text(template),
        (_, Reach::Any) => "anything".into(),
        (_, Reach::Hidden) => format!("one path {HIDDEN}"),
    }
}

/// A resource as a sentence, without its full stop: `Runs exactly
/// `git status``, `Reads anything under ~/notes/`.
fn reach_sentence(resource: &PermissionResourceConstraint, reach: &Reach) -> String {
    let verb = match resource.kind {
        PermissionResourceKind::Command => "Runs",
        PermissionResourceKind::Url => "Fetches",
        PermissionResourceKind::Query => "Searches for",
        _ => verbs(resource.access.as_ref()).1,
    };
    let object = match (&resource.kind, reach) {
        (PermissionResourceKind::Query, Reach::Exact(query)) => format!("“{}”", review_text(query)),
        _ => reach_object(resource, reach),
    };
    format!("{verb} {object}")
}

/// A resource the way a list row names it: `cargo test *`, `reading
/// ~/notes/`, `pages under docs.rs/ratatui/`.
fn reach_phrase(resource: &PermissionResourceConstraint, reach: &Reach) -> String {
    match (&resource.kind, reach) {
        (PermissionResourceKind::Command, Reach::Exact(value))
        | (PermissionResourceKind::Command, Reach::Pattern(value))
        | (PermissionResourceKind::Command, Reach::Template(value)) => review_text(value),
        (PermissionResourceKind::Command, Reach::Prefix(prefix)) => {
            format!("{}…", review_text(prefix))
        }
        (PermissionResourceKind::Command, Reach::Any) => "any command".into(),
        (PermissionResourceKind::Command, Reach::Hidden) => format!("a command {HIDDEN}"),
        (PermissionResourceKind::Url, Reach::Exact(url)) => url_place(&redact_url_query(url)),
        (PermissionResourceKind::Url, Reach::Hidden) => format!("a page {HIDDEN}"),
        (PermissionResourceKind::Url | PermissionResourceKind::Query, _) => {
            reach_object(resource, reach)
        }
        (_, Reach::Hidden) => format!("{} a path {HIDDEN}", verbs(resource.access.as_ref()).0),
        _ => format!(
            "{} {}",
            verbs(resource.access.as_ref()).0,
            reach_object(resource, reach)
        ),
    }
}

/// What a rule allows, in plain sentences, and whether every part of it
/// could be shown. `here` is the project the reader is in, so a command
/// started there says so.
pub(crate) fn rule_summary(
    rule: &StructuredPermissionRule,
    review: Option<&PermissionReview>,
    here: Option<&Path>,
) -> ScopeSummary {
    let mut summary = ScopeSummary {
        lines: Vec::new(),
        complete: review.is_some_and(|review| {
            review.resources.len() == rule.resources.len()
                && (matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained)
                    || review.input.is_some())
        }),
    };
    for (index, resource) in rule.resources.iter().enumerate() {
        let shown = shown(review, index);
        let reach = reach(resource, shown);
        summary.complete &= !matches!(reach, Reach::Hidden);
        let mut sentence = reach_sentence(resource, &reach);
        if resource.kind == PermissionResourceKind::Command {
            match start(resource, shown) {
                Start::Anywhere => sentence.push_str(", started in any folder"),
                Start::In(workdir) => {
                    sentence.push_str(&format!(", started in {}", tilde(&workdir)));
                    if here.is_some_and(|here| Path::new(&workdir) == here) {
                        sentence.push_str(THIS_PROJECT);
                    }
                }
                Start::Hidden => {
                    summary.complete = false;
                    sentence.push_str(&format!(", started in a folder {HIDDEN}"));
                }
            }
        }
        sentence.push('.');
        summary.lines.push(sentence);
        if resource.protected == Some(true) {
            summary.lines.push(PROTECTED_SCOPE.into());
        }
        if let Some(workdirs) =
            shown.and_then(|shown| shown.attributes.get(POSSIBLE_WORKDIRS_ATTRIBUTE))
        {
            summary.lines.push(review_text(workdirs));
        }
        summary.complete &= shown.is_some_and(|shown| {
            shown.attributes.len() == resource.attributes.len()
                && !shown
                    .attributes
                    .values()
                    .any(|value| value.contains(UNAVAILABLE))
        });
    }
    if let Some(family) = &rule.family {
        summary.lines.push(
            match family {
                PermissionCapabilityFamily::FilesystemBrowse => {
                    "Lists file and folder names only; never reads contents, writes, or runs commands."
                }
                PermissionCapabilityFamily::FilesystemRead => {
                    "Reads, lists, and searches; never writes or runs commands."
                }
                PermissionCapabilityFamily::McpServer => "Every tool on this MCP server.",
            }
            .into(),
        );
    }
    match rule.arguments {
        PermissionArgumentConstraint::Exact { .. } => summary.lines.push(EXACT_CALL.into()),
        PermissionArgumentConstraint::Selected { .. }
        | PermissionArgumentConstraint::SelectedDigest { .. } => {
            summary.lines.push(SELECTED_INPUTS.into())
        }
        PermissionArgumentConstraint::Unconstrained if rule.resources.is_empty() => {
            summary.lines.push(ANY_ARGUMENTS.into())
        }
        PermissionArgumentConstraint::Unconstrained => {}
    }
    if rule.resources.iter().any(|resource| {
        resource.kind == PermissionResourceKind::Command && !exact_selector(&resource.selector)
    }) {
        summary.lines.push(SHELL_REACH.into());
    }
    summary.complete &= summary.lines.iter().all(|line| complete_text(line));
    if !summary.complete {
        summary.lines.push(MISSING_SCOPE.into());
    }
    summary
}

/// A rule's scope the way a list row names it. A command started outside
/// `binding`, the project the rule belongs to, names its folder.
pub(crate) fn rule_phrase(
    rule: &StructuredPermissionRule,
    review: Option<&PermissionReview>,
    binding: Option<&Path>,
) -> String {
    let phrases: Vec<String> = rule
        .resources
        .iter()
        .enumerate()
        .map(|(index, resource)| {
            let shown = shown(review, index);
            let reach = reach(resource, shown);
            let mut phrase = reach_phrase(resource, &reach);
            if resource.kind == PermissionResourceKind::Command {
                match start(resource, shown) {
                    Start::Anywhere if matches!(reach, Reach::Any) => {
                        phrase = "any shell command".into()
                    }
                    Start::Anywhere => {}
                    Start::In(workdir)
                        if !matches!(reach, Reach::Any)
                            && binding.is_some_and(|binding| Path::new(&workdir) == binding) => {}
                    Start::In(workdir) => phrase.push_str(&format!(" in {}", folder(&workdir))),
                    Start::Hidden => phrase.push_str(&format!(" in a folder {HIDDEN}")),
                }
            }
            phrase
        })
        .collect();
    match phrases.as_slice() {
        [] => match (&rule.family, &rule.arguments) {
            (Some(PermissionCapabilityFamily::McpServer), _) => {
                "every tool on this MCP server".into()
            }
            (_, PermissionArgumentConstraint::Exact { .. }) => "this exact call".into(),
            (
                _,
                PermissionArgumentConstraint::Selected { .. }
                | PermissionArgumentConstraint::SelectedDigest { .. },
            ) => "calls with some inputs fixed".into(),
            (_, PermissionArgumentConstraint::Unconstrained) => "any arguments".into(),
        },
        shown if shown.len() > MAX_ROW_TARGETS => format!(
            "{} and {} more",
            shown[..MAX_ROW_TARGETS].join(", "),
            shown.len() - MAX_ROW_TARGETS
        ),
        shown => shown.join(", "),
    }
}

pub(super) fn option_summary(
    request: &PermissionRequest,
    option: &PermissionRuleOption,
) -> ScopeSummary {
    if let Some(definition) = offered_pattern(option) {
        return pattern_summary(definition);
    }
    rule_summary(
        &option.rule,
        Some(&review_for_rule(request, &option.rule)),
        request.presentation.project.as_deref(),
    )
}

/// The literal command line, or what the call does.
pub(super) fn requested_action(request: &PermissionRequest) -> String {
    let shell = request
        .resources
        .iter()
        .any(|resource| resource.kind == PermissionResourceKind::Command);
    if shell && let Some(command) = requested_command(request) {
        return command.to_owned();
    }
    match request.resources.as_slice() {
        [resource] if shell => resource.value.clone(),
        _ => request.presentation.action.clone(),
    }
}

fn requested_command(request: &PermissionRequest) -> Option<&str> {
    ["command", "command_text"]
        .iter()
        .find_map(|key| request.input.get(key).and_then(|value| value.as_str()))
}

/// Which tool asks and who provides it, as Details names it.
pub(super) fn tool_text(request: &PermissionRequest) -> String {
    tool_words(
        &request.tool.to_string(),
        &request.subject,
        &request.executor,
    )
}

/// A tool and who provides it: `shell (built-in)`, `search from MCP server
/// docs`.
pub(crate) fn tool_words(
    tool: &str,
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
) -> String {
    let tool = review_text(tool);
    match subject {
        PermissionSubject::Native { .. } if *executor == PermissionExecutorKind::Native => {
            format!("{tool} (built-in)")
        }
        PermissionSubject::Native { .. } => format!("{tool} (runs outside this computer)"),
        PermissionSubject::Mcp { server, .. } => {
            format!("{tool} from MCP server {}", review_text(server))
        }
        PermissionSubject::Lua { plugin, .. } => {
            format!("{tool} from plugin {}", review_text(plugin))
        }
        PermissionSubject::RemoteWorkcell { .. } | PermissionSubject::RemoteNative { .. } => {
            format!("{tool} on a remote workspace")
        }
        PermissionSubject::UnknownLegacy { .. } => format!("{tool} (unverified tool)"),
    }
}

pub(super) fn complete_text(text: &str) -> bool {
    !text.contains(TRUNCATED)
        && !text.contains(INCOMPLETE_REDACTION)
        && !text.contains(OMITTED_MARKER)
}

/// The rule a rung would store, for the advanced view.
pub(super) fn option_model(
    request: &PermissionRequest,
    option: &PermissionRuleOption,
    lifetime: PermissionLifetime,
) -> ScopeModel {
    let mut rule = option.rule.clone();
    rule.lifetime = lifetime;
    ScopeModel {
        source: ScopeSource::Live {
            rule: Box::new(rule),
            review: review_for_rule(request, &option.rule),
            project: request.presentation.project.clone(),
        },
        activity: ScopeActivity::Live,
    }
}

/// An edited template, for the advanced view.
pub(super) fn pattern_model(
    definition: Box<PatternDefinition>,
    lifetime: PermissionLifetime,
) -> ScopeModel {
    ScopeModel {
        source: ScopeSource::Pattern {
            definition,
            lifetime,
        },
        activity: ScopeActivity::Live,
    }
}
