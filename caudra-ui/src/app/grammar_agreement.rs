//! Every closed word the palette offers is one its handler accepts.

use caudra_providers::ThinkingConfig;
use caudra_storage::thinking::EFFORT_LEVELS;
use caudra_workspace::WorkspaceControlCommand;
use test_case::test_case;

use super::automation::parse_automations_command;
use super::autopilot::{AutopilotCommand, parse_autopilot_command};
use super::sandbox::parse_sandbox_command;
use super::tasks::parse_task_command;
use super::{is_goal_clear, search_tab};
use crate::components::command::grammar::{ArgKind, Subcommand};
use crate::components::command::words::{
    ADAPTIVE_ARG, CLEAR_ARG, LOG_ARG, MODEL_ARG, OFF_ARG, PROJECT_ARG, RUNS_ARG,
};
use crate::components::command::{BUILTIN_COMMANDS, SEARCH_COMMAND};
use crate::components::search_modal::SearchTab;
use crate::components::workflow_inspector::RunControl;
use crate::components::worktree_picker::WorktreeView;

const AN_ID: &str = "x1";
const THE_WORDS: &str = "some words";
const ACKNOWLEDGE: &str = "acknowledge";

fn subcommands(name: &str) -> &'static [Subcommand<'static>] {
    BUILTIN_COMMANDS
        .iter()
        .find(|command| command.name == name)
        .unwrap_or_else(|| panic!("{name} is not a built-in"))
        .args
        .subcommands
}

/// Every way to write each subcommand of `name`, with a value for each
/// slot it requires.
fn lines(name: &str) -> Vec<(&'static str, String)> {
    subcommands(name)
        .iter()
        .flat_map(|sub| {
            let required: Vec<&str> = sub
                .node
                .args
                .iter()
                .filter(|arg| arg.required)
                .map(|arg| match arg.kind {
                    ArgKind::Word(choices) => choices[0].value,
                    ArgKind::Value(_) | ArgKind::Text(_) => AN_ID,
                })
                .collect();
            std::iter::once(sub.name)
                .chain(sub.aliases.iter().copied())
                .map(move |word| {
                    let line = std::iter::once(word)
                        .chain(required.iter().copied())
                        .collect::<Vec<_>>()
                        .join(" ");
                    (sub.name, line)
                })
        })
        .collect()
}

#[test]
fn every_worktree_subcommand_parses() {
    for (_, line) in lines("/worktree") {
        assert!(WorktreeView::parse(&line).is_ok(), "{line}");
    }
}

#[test]
fn every_autopilot_subcommand_parses() {
    for (_, line) in lines("/autopilot") {
        let command = parse_autopilot_command(&line);
        assert!(
            !matches!(
                command,
                AutopilotCommand::Goal(_) | AutopilotCommand::Engage
            ),
            "{line}"
        );
    }
}

#[test]
fn every_goal_subcommand_is_one_goal_reads() {
    for (name, line) in lines("/goal") {
        match name {
            CLEAR_ARG => assert!(is_goal_clear(&line), "{line}"),
            MODEL_ARG | LOG_ARG => assert!(line.eq_ignore_ascii_case(name)),
            _ => panic!("/goal {name} has no handler"),
        }
    }
}

#[test]
fn every_automations_subcommand_parses() {
    for (_, line) in lines("/automations") {
        assert!(
            matches!(parse_automations_command(&line), Ok(Some(_))),
            "{line}"
        );
    }
}

#[test]
fn every_tasks_subcommand_parses() {
    for (_, line) in lines("/tasks") {
        assert!(parse_task_command(&line).is_some(), "{line}");
    }
}

#[test]
fn every_sandbox_subcommand_parses() {
    for (_, line) in lines("/sandbox") {
        assert!(parse_sandbox_command(&line).is_some(), "{line}");
    }
}

#[test]
fn every_workflow_control_parses() {
    for (_, line) in lines("/workflow") {
        let verb = line.split_whitespace().next().unwrap_or_default();
        assert!(
            RunControl::parse(verb).is_some() || line == RUNS_ARG,
            "{line}"
        );
    }
}

#[test]
fn every_remote_subcommand_parses() {
    for (name, line) in lines("/remote") {
        match WorkspaceControlCommand::parse(&line) {
            Ok(_) => {}
            Err(error) => assert!(name == ACKNOWLEDGE && !error.starts_with("Usage"), "{line}"),
        }
    }
}

#[test]
fn every_search_tab_is_read_as_one() {
    for (_, line) in lines(SEARCH_COMMAND) {
        let args = format!("{line} {THE_WORDS}");
        let (tab, words) = search_tab(&args);
        assert_eq!(words, THE_WORDS);
        assert!(tab != SearchTab::Project || line == PROJECT_ARG, "{line}");
    }
}

#[test_case(OFF_ARG ; "off")]
#[test_case(ADAPTIVE_ARG ; "adaptive")]
fn thinking_words_parse(level: &str) {
    assert!(ThinkingConfig::parse(level, &ThinkingConfig::Off).is_ok());
}

#[test]
fn every_effort_level_parses() {
    for level in EFFORT_LEVELS {
        assert!(
            ThinkingConfig::parse(level, &ThinkingConfig::Off).is_ok(),
            "{level}"
        );
    }
}
