use agent_client_protocol_schema::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
};
use caudra_agent::permissions::{PermissionAnswer, PermissionLifetime, PermissionRequest};

const ALLOW_ONCE_ID: &str = "allow_once";
const ALLOW_ALWAYS_ID: &str = "allow_always";
const REJECT_ONCE_ID: &str = "reject_once";
const REJECT_ALWAYS_ID: &str = "reject_always";

pub fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ONCE_ID),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ALWAYS_ID),
            "Allow exact call for conversation",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ONCE_ID),
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ALWAYS_ID),
            "Reject exact call for project",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

pub fn outcome_to_answer(
    outcome: &RequestPermissionOutcome,
    exact_project_deny: bool,
) -> PermissionAnswer {
    match outcome {
        RequestPermissionOutcome::Cancelled => PermissionAnswer::Deny,
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW_ONCE_ID => exact_allow(PermissionLifetime::Once),
            ALLOW_ALWAYS_ID => exact_allow(PermissionLifetime::Conversation),
            REJECT_ONCE_ID => PermissionAnswer::Deny,
            REJECT_ALWAYS_ID if exact_project_deny => PermissionAnswer::DenyAlwaysLocal,
            REJECT_ALWAYS_ID => PermissionAnswer::Deny,
            _ => PermissionAnswer::Deny,
        },
        _ => PermissionAnswer::Deny,
    }
}

fn exact_allow(lifetime: PermissionLifetime) -> PermissionAnswer {
    PermissionAnswer::AllowOption {
        option_id: "allow_exact".into(),
        lifetime,
    }
}

pub fn exact_project_deny_is_representable(request: &PermissionRequest) -> bool {
    request.options.iter().any(|option| {
        option.id == "deny_exact"
            && option
                .allowed_lifetimes
                .contains(&caudra_agent::permissions::PermissionLifetime::Project)
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use caudra_config::ToolKey;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    #[test_case(vec!["cargo test".into()] ; "exact_scope")]
    #[test_case(vec!["cargo *".into()] ; "broad_command_scope")]
    #[test_case(vec!["/project/src/**".into()] ; "broad_path_scope")]
    #[test_case(Vec::new() ; "missing_scope")]
    fn reject_always_uses_the_host_generated_exact_option(scopes: Vec<String>) {
        let request = PermissionRequest::from_legacy(
            "request-id".into(),
            ToolKey::native("bash"),
            scopes,
            json!({"command": "cargo test"}),
            Path::new("/project"),
            false,
        );

        assert!(exact_project_deny_is_representable(&request));
    }
}
