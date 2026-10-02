use super::{
    PermissionAnswer, PermissionLifetime, PermissionRequest, PermissionSubject, PromptReason,
    ResourceCoverage, lifetime_name,
};
use super::{PermissionArgumentConstraint, PermissionResourceKind, PermissionResourceSelector};
use std::borrow::Cow;
use std::fmt::Write;

/// Wide events for prompt analysis. They land in the ordinary JSON log, which
/// already carries scopes and command text, rather than in telemetry, so the
/// answer to "what keeps prompting me" does not depend on an exporter.
pub(super) const PERMISSION_LOG_TARGET: &str = "caudra::permission";

pub(super) const PROMPT_LOG_MAX_RESOURCES: usize = 8;

pub(super) const PROMPT_LOG_MAX_VALUE_CHARS: usize = 200;

/// The `lifetime` of a composed answer whose remembered rows differ.
pub(super) const MIXED_LIFETIMES: &str = "mixed";

/// Why the request could not be settled from stored authority. Ordered by
/// precedence: the first that applies is reported.
pub(super) const PROMPT_REASON_FORCED: &str = "force_prompt";

pub(super) const PROMPT_REASON_PROTECTED: &str = "protected";

pub(super) const PROMPT_REASON_REQUIRES_PROMPT: &str = "requires_prompt";

pub(super) const PROMPT_REASON_ASK_RULE: &str = "ask_rule";

pub(super) const PROMPT_REASON_UNCOVERED: &str = "uncovered";
pub(super) const PROMPT_MESSAGE_PLAN: &str =
    "Plan mode requires approval for this operation; project and global grants are unavailable.";
pub(super) const PROMPT_MESSAGE_FORCED: &str = "This tool call explicitly requires approval.";
pub(super) const PROMPT_MESSAGE_PROTECTED: &str = "A protected resource needs explicit approval.";
pub(super) const PROMPT_MESSAGE_REQUIRES_PROMPT: &str =
    "The prepared operation requires review before it can proceed.";
pub(super) const PROMPT_MESSAGE_ASK: &str =
    "Current permission policy asks for approval of this operation.";
pub(super) const PROMPT_MESSAGE_UNCOVERED: &str =
    "No current permission rule covers this operation.";

pub(super) fn bounded_log_value(value: &str) -> String {
    value
        .chars()
        .flat_map(char::escape_default)
        .take(PROMPT_LOG_MAX_VALUE_CHARS)
        .collect()
}

pub(super) fn answer_scope_kind(
    answer: &PermissionAnswer,
    request: &PermissionRequest,
) -> &'static str {
    let id = match answer {
        PermissionAnswer::AllowOption { option_id, .. } => option_id.as_str(),
        PermissionAnswer::AllowComposed { .. } => return "composed",
        PermissionAnswer::AllowOnce
        | PermissionAnswer::AllowSession
        | PermissionAnswer::AllowAlwaysLocal
        | PermissionAnswer::AllowAlwaysGlobal => return "exact_input",
        _ => return "deny",
    };
    let Some(option) = request.options.iter().find(|option| option.id == id) else {
        return "unavailable";
    };
    if matches!(
        option.rule.arguments,
        PermissionArgumentConstraint::Exact { .. }
    ) {
        return "exact_input";
    }
    match option
        .rule
        .resources
        .first()
        .map(|resource| &resource.selector)
    {
        Some(
            PermissionResourceSelector::Exact { .. }
            | PermissionResourceSelector::Digest { .. }
            | PermissionResourceSelector::RemoteResource { .. },
        ) => "exact_resource",
        Some(
            PermissionResourceSelector::Subtree { .. }
            | PermissionResourceSelector::FilesystemSubtreeDigest { .. }
            | PermissionResourceSelector::UrlSubtreeDigest { .. }
            | PermissionResourceSelector::RemoteSubtree { .. },
        ) => "subtree",
        Some(PermissionResourceSelector::UrlOriginDigest { .. }) => "url_origin",
        Some(PermissionResourceSelector::CommandPattern { .. }) => "command_pattern",
        Some(PermissionResourceSelector::CommandTemplate { .. }) => "command_template",
        Some(PermissionResourceSelector::Prefix { .. }) => "prefix",
        Some(PermissionResourceSelector::Any) | None => "blanket",
    }
}

fn resource_kind_name(kind: &PermissionResourceKind) -> &'static str {
    match kind {
        PermissionResourceKind::File => "File",
        PermissionResourceKind::Directory => "Directory",
        PermissionResourceKind::Url => "Url",
        PermissionResourceKind::Command => "Command",
        PermissionResourceKind::Query => "Query",
        PermissionResourceKind::RemoteFile { .. } => "RemoteFile",
        PermissionResourceKind::RemoteDirectory { .. } => "RemoteDirectory",
        PermissionResourceKind::RemoteResource { .. } => "RemoteResource",
        PermissionResourceKind::Custom { .. } => "Custom",
    }
}

pub(super) fn subject_kind_and_contract(subject: &PermissionSubject) -> (&str, &str) {
    match subject {
        PermissionSubject::Native { owner, contract } => (owner, contract),
        PermissionSubject::Lua {
            plugin, contract, ..
        } => (plugin, contract),
        PermissionSubject::Mcp {
            server, contract, ..
        } => (server, contract),
        PermissionSubject::RemoteWorkcell {
            identity, contract, ..
        } => (identity.authority.server_id(), contract),
        PermissionSubject::RemoteNative {
            owner, contract, ..
        } => (owner, contract),
        PermissionSubject::UnknownLegacy { identity } => (identity, ""),
    }
}

/// Answer kind, chosen option, and granted lifetime as separate fields, so
/// prompt analysis can group by lifetime without parsing `encode()`. Denials
/// carry no lifetime because a `deny_always_*` writes its rule elsewhere.
pub(super) fn answer_log_fields(
    answer: &PermissionAnswer,
) -> (&'static str, Cow<'_, str>, &'static str) {
    let none = Cow::Borrowed("");
    match answer {
        PermissionAnswer::AllowOnce => ("allow", none, lifetime_name(&PermissionLifetime::Once)),
        PermissionAnswer::AllowSession => (
            "allow_session",
            none,
            lifetime_name(&PermissionLifetime::Conversation),
        ),
        PermissionAnswer::AllowAlwaysLocal => (
            "allow_always_local",
            none,
            lifetime_name(&PermissionLifetime::Project),
        ),
        PermissionAnswer::AllowAlwaysGlobal => (
            "allow_always_global",
            none,
            lifetime_name(&PermissionLifetime::Global),
        ),
        PermissionAnswer::AllowOption {
            option_id,
            lifetime,
        } => (
            "allow_option",
            Cow::Borrowed(option_id.as_str()),
            lifetime_name(lifetime),
        ),
        // The rungs themselves are not named: what matters for grouping is how
        // much of the request an answer chose to remember, and for how long.
        PermissionAnswer::AllowComposed { rows } => {
            let mut lifetimes = rows.iter().flatten().map(|row| &row.lifetime);
            let lifetime = match lifetimes.next() {
                None => lifetime_name(&PermissionLifetime::Once),
                Some(first) if lifetimes.all(|lifetime| lifetime == first) => lifetime_name(first),
                Some(_) => MIXED_LIFETIMES,
            };
            (
                "allow_composed",
                Cow::Owned(format!(
                    "{}/{} rows",
                    rows.iter().filter(|row| row.is_some()).count(),
                    rows.len()
                )),
                lifetime,
            )
        }
        PermissionAnswer::Deny => ("deny", none, ""),
        PermissionAnswer::DenyWithGuidance(_) => ("deny_guidance", none, ""),
        PermissionAnswer::DenyAlwaysLocal => ("deny_always_local", none, ""),
        PermissionAnswer::DenyAlwaysGlobal => ("deny_always_global", none, ""),
    }
}

/// The first reason that applies, so a log line names one cause rather than a
/// set. `ask_rule` covers both a builtin ask family and a configured ask;
/// telling them apart would mean widening what `request_coverage` returns.
pub(super) fn prompt_forcing_reason(
    request: &PermissionRequest,
    coverage: &[Option<ResourceCoverage>],
    forced: bool,
    ask_rule: bool,
) -> &'static str {
    let uncovered = || {
        request
            .resources
            .iter()
            .enumerate()
            .filter(move |(index, _)| coverage.get(*index).is_none_or(Option::is_none))
    };
    if forced {
        PROMPT_REASON_FORCED
    } else if uncovered().any(|(_, resource)| resource.protected) {
        PROMPT_REASON_PROTECTED
    } else if uncovered().any(|(_, resource)| resource.requires_prompt) {
        PROMPT_REASON_REQUIRES_PROMPT
    } else if ask_rule {
        PROMPT_REASON_ASK_RULE
    } else {
        PROMPT_REASON_UNCOVERED
    }
}

/// The prompt's reason as typed data. An ask rule is named by the authority
/// that decided it, so the pattern comes from typed coverage and never from
/// text a client rendered.
pub(super) fn prompt_reason(
    request: &PermissionRequest,
    coverage: &[Option<ResourceCoverage>],
    forced: bool,
    asking: Option<&ResourceCoverage>,
    plan_scoped: bool,
) -> PromptReason {
    if plan_scoped {
        return PromptReason::Plan;
    }
    match (
        prompt_forcing_reason(request, coverage, forced, asking.is_some()),
        asking,
    ) {
        (PROMPT_REASON_FORCED, _) => PromptReason::Forced,
        (PROMPT_REASON_PROTECTED, _) => PromptReason::Protected,
        (PROMPT_REASON_REQUIRES_PROMPT, _) => PromptReason::RequiresReview,
        (PROMPT_REASON_ASK_RULE, Some(asking)) => PromptReason::AskRule {
            origin: asking.origin,
            pattern: asking.authority.clone(),
        },
        _ => PromptReason::Uncovered,
    }
}

pub(super) fn prompt_reason_message(reason: &PromptReason) -> &'static str {
    match reason {
        PromptReason::Plan => PROMPT_MESSAGE_PLAN,
        PromptReason::Forced => PROMPT_MESSAGE_FORCED,
        PromptReason::Protected => PROMPT_MESSAGE_PROTECTED,
        PromptReason::RequiresReview => PROMPT_MESSAGE_REQUIRES_PROMPT,
        PromptReason::AskRule { .. } => PROMPT_MESSAGE_ASK,
        PromptReason::Uncovered => PROMPT_MESSAGE_UNCOVERED,
    }
}

/// Only the resources that actually forced the prompt, capped, so a chain of
/// twenty already-approved commands does not bury the one that is new.
pub(super) fn uncovered_resource_summary(
    request: &PermissionRequest,
    coverage: &[Option<ResourceCoverage>],
) -> String {
    let mut summary = String::new();
    let uncovered = request
        .resources
        .iter()
        .enumerate()
        .filter(|(index, _)| coverage.get(*index).is_none_or(Option::is_none));
    for (count, (_, resource)) in uncovered.enumerate() {
        if count == PROMPT_LOG_MAX_RESOURCES {
            let _ = write!(summary, ", ...");
            break;
        }
        if count > 0 {
            summary.push_str(", ");
        }
        let value = bounded_log_value(&resource.value);
        let _ = write!(summary, "{}:{value}", resource_kind_name(&resource.kind));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::{MIXED_LIFETIMES, bounded_log_value};
    use crate::permissions::PermissionResourceKind;
    use std::borrow::Cow;

    use test_case::test_case;

    use crate::permissions::tests::{composed_answer, log_coverage, log_request, log_resource};
    use crate::permissions::{
        PROMPT_LOG_MAX_RESOURCES, PROMPT_LOG_MAX_VALUE_CHARS, PermissionAnswer, PermissionLifetime,
        answer_log_fields, uncovered_resource_summary,
    };

    #[test_case("line\nnext", "line\\nnext"; "newline")]
    #[test_case("\u{1b}[2J", "\\u{1b}[2J"; "terminal_control")]
    fn diagnostic_values_escape_terminal_controls(value: &str, expected: &str) {
        assert_eq!(bounded_log_value(value), expected);
    }

    #[test_case("untrusted-kind"; "custom_kind")]
    fn diagnostic_resource_kind_never_includes_untrusted_metadata(name: &str) {
        let mut resource = log_resource("brief", false, false);
        resource.kind = PermissionResourceKind::Custom {
            name: name.repeat(PROMPT_LOG_MAX_VALUE_CHARS),
        };
        let request = log_request(vec![resource]);
        const EXPECTED: &str = "Custom:brief";
        assert_eq!(uncovered_resource_summary(&request, &[None]), EXPECTED);
    }
    #[test]
    fn uncovered_resource_summary_lists_only_uncovered_resources() {
        let request = log_request(vec![
            log_resource("already granted", false, false),
            log_resource("brand new", false, false),
        ]);

        let summary = uncovered_resource_summary(&request, &[log_coverage(), None]);

        assert_eq!(summary, "Command:brand new");
    }

    #[test]
    fn uncovered_resource_summary_caps_the_resource_count() {
        let resources = (0..PROMPT_LOG_MAX_RESOURCES + 3)
            .map(|index| log_resource(&format!("cmd{index}"), false, false))
            .collect();
        let request = log_request(resources);
        let coverage = vec![None; PROMPT_LOG_MAX_RESOURCES + 3];

        let summary = uncovered_resource_summary(&request, &coverage);

        assert!(summary.ends_with(", ..."), "{summary}");
        assert_eq!(
            summary.matches("Command:").count(),
            PROMPT_LOG_MAX_RESOURCES
        );
    }

    #[test]
    fn uncovered_resource_summary_truncates_a_long_value() {
        let value = "x".repeat(PROMPT_LOG_MAX_VALUE_CHARS * 2);
        let request = log_request(vec![log_resource(&value, false, false)]);

        let summary = uncovered_resource_summary(&request, &[None]);

        assert_eq!(
            summary,
            format!("Command:{}", "x".repeat(PROMPT_LOG_MAX_VALUE_CHARS))
        );
    }

    #[test_case(PermissionAnswer::AllowOnce, "allow", "", "once")]
    #[test_case(PermissionAnswer::AllowSession, "allow_session", "", "conversation")]
    #[test_case(
        PermissionAnswer::AllowAlwaysLocal,
        "allow_always_local",
        "",
        "project"
    )]
    #[test_case(
        PermissionAnswer::AllowAlwaysGlobal,
        "allow_always_global",
        "",
        "global"
    )]
    #[test_case(PermissionAnswer::Deny, "deny", "", "")]
    #[test_case(PermissionAnswer::DenyAlwaysGlobal, "deny_always_global", "", "")]
    fn answer_log_fields_splits_the_answer_into_groupable_fields(
        answer: PermissionAnswer,
        expected_answer: &str,
        expected_option: &str,
        expected_lifetime: &str,
    ) {
        assert_eq!(
            answer_log_fields(&answer),
            (
                expected_answer,
                Cow::Borrowed(expected_option),
                expected_lifetime
            )
        );
    }

    #[test]
    fn answer_log_fields_reports_the_option_and_its_chosen_lifetime() {
        let answer = PermissionAnswer::AllowOption {
            option_id: "allow_subtree".into(),
            lifetime: PermissionLifetime::Project,
        };

        assert_eq!(
            answer_log_fields(&answer),
            ("allow_option", Cow::Borrowed("allow_subtree"), "project")
        );
    }

    #[test_case(&[None, None], "0/2 rows", "once"; "nothing_remembered")]
    #[test_case(&[Some(PermissionLifetime::Project), None], "1/2 rows", "project"; "one_lifetime")]
    #[test_case(&[Some(PermissionLifetime::Project), Some(PermissionLifetime::Conversation)], "2/2 rows", MIXED_LIFETIMES; "rows_that_differ")]
    fn answer_log_fields_report_how_long_a_composed_answer_lasts(
        lifetimes: &[Option<PermissionLifetime>],
        expected_rows: &str,
        expected_lifetime: &str,
    ) {
        assert_eq!(
            answer_log_fields(&composed_answer(lifetimes)),
            (
                "allow_composed",
                Cow::Borrowed(expected_rows),
                expected_lifetime
            )
        );
    }
}
