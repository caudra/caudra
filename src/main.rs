#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use workcell_mcp::{
    cli::RawOptions, config::resolve_web_configuration_with, environment::StartupEnvironment,
    logging, resolve_shell_policy, run, validate_http_authentication,
};
use workcell_mcp_web::WebsearchBackend;

#[tokio::main]
async fn main() -> ExitCode {
    let raw = RawOptions::parse();
    let environment = match StartupEnvironment::load(raw.env_file.as_deref()) {
        Ok(environment) => environment,
        Err(error) => return fail(error),
    };
    let options = match raw.resolve(&environment) {
        Ok(options) => options,
        Err(error) => return fail(error),
    };
    let web = resolve_web_configuration_with(|name| environment.read(name).map_err(|_| ()));
    let shell_policy = match resolve_shell_policy(&options) {
        Ok(policy) => policy,
        Err(error) => return fail(error),
    };
    let shell_policy_summary = shell_policy.summary();
    let environment_token = match environment.read("WORKCELL_MCP_HTTP_TOKEN") {
        Ok(token) => token,
        Err(_) => return fail("WORKCELL_MCP_HTTP_TOKEN must be valid UTF-8"),
    };
    let authentication = match validate_http_authentication(&options, environment_token) {
        Ok(authentication) => authentication,
        Err(error) => return fail(error),
    };
    let logging = match logging::initialize_with(|name| environment.read(name).ok().flatten()) {
        Ok(logging) => logging,
        Err(error) => return fail(error),
    };
    tracing::info!(
        operation = "mcp.starting",
        transport = options.transport.as_str(),
        http_bind = options.http_bind.as_str(),
        tool_groups = ?options.groups.iter().map(|group| group.as_str()).collect::<Vec<_>>(),
        allow_write = options.allow_write,
        web_icons = options.web_icons,
        shell_policy_configured = options.shell_policy_file.is_some(),
        shell_yolo = options.yolo,
        websearch_backend = web.backend().map(WebsearchBackend::as_str),
        websearch_status = web.status(),
        authenticated_http = authentication.is_some(),
        log_level = logging.level(),
        log_format = logging.format(),
        "Workcell MCP starting"
    );
    if options
        .groups
        .contains(&workcell_mcp::cli::ToolGroup::Shell)
    {
        tracing::info!(
            operation = "shell.policy.loaded",
            source = if options.shell_policy_file.is_some() {
                "file"
            } else if options.yolo {
                "yolo"
            } else {
                "built-in"
            },
            default_decision = shell_policy_summary.default_decision,
            allow_rule_count = shell_policy_summary.allow_rule_count,
            deny_rule_count = shell_policy_summary.deny_rule_count,
            yolo = shell_policy_summary.yolo,
            "Shell permission policy parsed"
        );
    }
    match run(options, web, authentication, shell_policy).await {
        Ok(outcome) => {
            tracing::info!(
                operation = "mcp.stopped",
                outcome = outcome.as_str(),
                "Workcell MCP stopped"
            );
            if outcome.requires_immediate_process_exit() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => fail(error),
    }
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("workcell-mcp: {error}");
    ExitCode::FAILURE
}
