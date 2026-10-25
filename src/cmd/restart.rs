//! Starting the binary an update installed on the sessions this process ran,
//! with the run-wide flags it was started with.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitCode};

use clap::CommandFactory;
use clap::parser::ValueSource;

use crate::cli::Cli;

const CONTINUE: &str = "--continue";
const RESTART_FAILED: &str = "Could not start the updated Caudra";
const RESUME_HINT: &str = "run `caudra --continue` to reopen your sessions";

/// Flags that shape the whole run rather than its first session, which the
/// restarted process needs again.
const FORWARDED: &[&str] = &[
    "sandbox",
    "sandbox_resume",
    "profile",
    "endpoint",
    "cwd",
    "credential_ref",
    "ephemeral",
    "no_snapshots",
    "accounts",
    "no_commands",
    "no_rtk",
    "no_plugins",
    "no_jit",
    "yolo",
    "auto",
    "allowed_tools",
    "disallowed_tools",
    "system_prompt",
    "system_prompt_profile",
    "append_system_prompt",
    "permission_mode",
];

/// Flags for the first session, for print mode, or that the sessions
/// already keep, which a restart onto saved sessions must not repeat.
#[cfg(test)]
const DROPPED: &[&str] = &[
    "print",
    "prompt",
    "images",
    "model",
    "verbose",
    "continue_session",
    "session",
    "output_format",
    "input_format",
    "exit_on_done",
    "name",
    "topics",
    "groups",
    "receive_broadcasts",
    "automations",
    "autopilot",
    "session_id",
    "fork_session",
    "max_turns",
    "include_partial_messages",
    "permission_prompt_tool",
    "fallback_model",
    "settings",
    "setting_sources",
    "add_dir",
    "strict_mcp_config",
    "include_hook_events",
    "mcp_config",
    "tools",
    "betas",
    "max_thinking_tokens",
    "effort",
    "json_schema",
    "max_budget_usd",
    "thinking",
    "thinking_display",
];

/// How the process ends once every guard of the command has dropped.
pub enum Finish {
    Exit(ExitCode),
    /// Replaces this process with the command an update prepared.
    Restart(Command),
}

impl From<ExitCode> for Finish {
    fn from(code: ExitCode) -> Self {
        Self::Exit(code)
    }
}

/// `binary` resumes this directory's tabs with the forwarded flags of `argv`,
/// the arguments this process was started with.
pub(super) fn restart_command(
    binary: &Path,
    argv: impl IntoIterator<Item = OsString>,
) -> Result<Command, clap::Error> {
    let mut command = Command::new(binary);
    command.args(forwarded_args(argv)?);
    Ok(command)
}

fn forwarded_args(argv: impl IntoIterator<Item = OsString>) -> Result<Vec<OsString>, clap::Error> {
    let cli = Cli::command();
    let matches = cli.clone().try_get_matches_from(argv)?;
    let mut args = vec![OsString::from(CONTINUE)];
    for arg in cli
        .get_arguments()
        .filter(|arg| FORWARDED.contains(&arg.get_id().as_str()))
    {
        let id = arg.get_id().as_str();
        let Some(long) = arg.get_long() else {
            continue;
        };
        if matches.value_source(id) != Some(ValueSource::CommandLine) {
            continue;
        }
        if !arg.get_action().takes_values() {
            args.push(format!("--{long}").into());
            continue;
        }
        for value in matches.get_raw(id).into_iter().flatten() {
            let mut flag = OsString::from(format!("--{long}="));
            flag.push(value);
            args.push(flag);
        }
    }
    Ok(args)
}

/// Only returns when the restart failed, saying how to finish it by hand.
pub fn restart(mut command: Command) -> ExitCode {
    #[cfg(unix)]
    let error = {
        use std::os::unix::process::CommandExt;
        command.exec()
    };
    #[cfg(not(unix))]
    let error = match command.status() {
        Ok(status) => {
            return ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1));
        }
        Err(error) => error,
    };
    eprintln!("{RESTART_FAILED}: {error}; {RESUME_HINT}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsString;

    use clap::CommandFactory;
    use clap::builder::ArgAction;
    use test_case::test_case;

    use super::{CONTINUE, DROPPED, FORWARDED, forwarded_args};
    use crate::cli::Cli;

    const UNCLASSIFIED: &str = "every flag is either forwarded on restart or dropped";
    const NO_LONG: &str = "a forwarded flag needs a long name to be repeated";
    const UNSUPPORTED_ACTION: &str = "a forwarded flag is a switch or takes values";

    fn args(argv: &[&str]) -> Vec<String> {
        forwarded_args(
            std::iter::once("caudra")
                .chain(argv.iter().copied())
                .map(OsString::from),
        )
        .unwrap()
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect()
    }

    #[test]
    fn every_flag_is_classified_once() {
        let forwarded: HashSet<_> = FORWARDED.iter().copied().collect();
        let dropped: HashSet<_> = DROPPED.iter().copied().collect();
        assert!(forwarded.is_disjoint(&dropped), "{UNCLASSIFIED}");
        let cli = Cli::command();
        let ids: HashSet<_> = cli
            .get_arguments()
            .map(|arg| arg.get_id().as_str())
            .collect();
        let classified: HashSet<_> = forwarded.union(&dropped).copied().collect();
        assert_eq!(ids, classified, "{UNCLASSIFIED}");
        for arg in cli
            .get_arguments()
            .filter(|arg| forwarded.contains(arg.get_id().as_str()))
        {
            assert!(arg.get_long().is_some(), "{NO_LONG}: {}", arg.get_id());
            assert!(
                matches!(arg.get_action(), ArgAction::SetTrue) || arg.get_action().takes_values(),
                "{UNSUPPORTED_ACTION}: {}",
                arg.get_id()
            );
        }
    }

    #[test_case(&[], &[CONTINUE]; "bare_start_continues")]
    #[test_case(&["--continue"], &[CONTINUE]; "continue_is_not_repeated")]
    #[test_case(&["--yolo", "--prompt", "x", "-m", "a/b"], &[CONTINUE, "--yolo"]; "first_session_flags_drop")]
    #[test_case(&["--session", "abc"], &[CONTINUE]; "a_resumed_session_reopens_with_its_tabs")]
    #[test_case(&["--account", "a:b", "--account", "c:d"], &[CONTINUE, "--account=a:b", "--account=c:d"]; "repeated_values_repeat")]
    #[test_case(&["--append-system-prompt=-be brief"], &[CONTINUE, "--append-system-prompt=-be brief"]; "hyphen_values_stay_values")]
    #[test_case(&["--sandbox", "box", "--sandbox-resume"], &[CONTINUE, "--sandbox=box", "--sandbox-resume"]; "the_sandbox_follows")]
    #[test_case(&["--dangerously-skip-permissions"], &[CONTINUE, "--yolo"]; "aliases_forward_by_their_name")]
    fn restart_keeps_run_wide_flags(argv: &[&str], expected: &[&str]) {
        assert_eq!(args(argv), expected);
    }
}
