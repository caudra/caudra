use std::{collections::BTreeMap, path::Path};

use caudra_agent::permissions::{
    canonical_json_sha256,
    pattern_recognition::{
        CommandObservation, InvocationOutcome, OBSERVATION_SCHEMA_VERSION, ObservationProvenance,
        ObservationSource, ObservationVerification, ShellEffectStatus,
    },
};
use caudra_storage::permission_patterns::{ArgumentRole, PatternContext};
use serde::Serialize;
use serde_json::json;
use workcell::shell::{
    ShellCommandScope, ShellWord,
    bash::{
        BashCommand, BashCommandContext, BashCommandContexts, BashCoverageKind, BashCwdSet,
        BashDiagnosticKind, BashParseError, BashProgram, BashRedirect, BashRedirectKind,
        parse_bash,
    },
};

use crate::read_only_shell::decimal;

pub use caudra_agent::permissions::COMMAND_OBSERVATION_ATTRIBUTE;
pub use workcell::shell::bash::{
    BashContextAssumptions, BashContextIssue, BashOperatorKind, BashSpan,
};

pub const MAX_PATTERN_DIAGNOSTIC_DETAILS: usize = 128;
const TOOL_IDENTITY: &str = "workcell/shell";
const ANALYSIS_VERSION: &str = "caudra-shell-patterns/v1";
const PREPARED_SOURCE: &str = "prepared-shell";
const IMPORTED_SOURCE: &str = "imported-shell";
const NULL_DEVICE: &str = "/dev/null";
const OPAQUE_WRAPPERS: &[&str] = &[
    "command", "builtin", "env", "sudo", "doas", "su", "xargs", "eval", "source", ".", "bash",
    "sh", "time", "coproc",
];
const PAYLOAD_EXECUTABLES: &[&str] = &[
    "cd",
    "exec",
    "ash",
    "dash",
    "zsh",
    "ksh",
    "csh",
    "tcsh",
    "fish",
    "cmd",
    "powershell",
    "pwsh",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "jq",
    "yq",
    "ssh",
    "sshpass",
    "rsh",
    "mosh",
    "expect",
    "tclsh",
    "wish",
    "osascript",
    "deno",
    "bun",
    "npx",
    "ts-node",
    "tsx",
    "parallel",
    "bc",
    "dc",
    "sqlite3",
    "psql",
    "mysql",
    "r",
    "rscript",
];
const VERSIONED_INTERPRETERS: &[&str] = &[
    "python",
    "pypy",
    "ipython",
    "jython",
    "micropython",
    "node",
    "nodejs",
    "perl",
    "raku",
    "ruby",
    "irb",
    "lua",
    "luajit",
    "php",
    "julia",
];
const SCRIPT_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "ksh", "fish", "py", "pyw", "js", "mjs", "cjs", "ts", "pl", "rb", "lua",
    "php", "ps1", "bat", "cmd",
];
const PAYLOAD_FLAGS: &[&str] = &[
    "--eval",
    "--evaluate",
    "--execute",
    "--exec",
    "--command",
    "--script",
    "--source",
    "--file",
    "--expression",
    "--config",
    "--configuration",
    "--config-file",
    "--config-env",
    "--config-path",
    "--rcfile",
    "--init-file",
    "--startup-file",
    "--response-file",
    "--args-file",
    "--argument-file",
    "--options-file",
    "--flags-file",
    "--flagfile",
    "--pre",
    "--pre-glob",
    "--rsh",
    "--exec-path",
    "--ext-diff",
    "--textconv",
    "--pager",
    "--use-compress-program",
    "--compress-program",
    "--upload-pack",
    "--receive-pack",
    "--rsync-path",
];
const SENSITIVE_MARKERS: &[&str] = &[
    "auth",
    "token",
    "password",
    "passwd",
    "secret",
    "credential",
    "apikey",
    "privatekey",
    "cookie",
    "header",
    "signature",
    "bearer",
    "basic",
    "-----begin",
    "sk-",
    "ghp_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
];
const SENSITIVE_FLAGS: &[&str] = &["-H", "-u", "-d", "--user", "--data", "--json"];

pub struct PatternCallAnalysis {
    pub observations: Vec<CommandObservation>,
    pub contexts: BashCommandContexts,
    pub requires_exact_source: bool,
    pub diagnostics: PatternCallDiagnostics,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PatternCallDiagnostics {
    pub represented_command_count: usize,
    pub observed_command_count: usize,
    pub omission_counts: BTreeMap<PatternOmissionReason, usize>,
    pub omissions: Vec<PatternCommandOmission>,
    pub omissions_truncated: usize,
    pub obligations: PatternSourceObligations,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PatternCommandOmission {
    pub span: BashSpan,
    pub reason: PatternOmissionReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternOmissionReason {
    IncompleteSource,
    UnrepresentableScope,
    ShellAssignments,
    ShellRedirects,
    NonStaticArguments,
    ExecutableIdentity,
    OpaqueWrapper,
    InterpretedExecutable,
    PotentialPayloadArgument,
    SensitiveArguments,
    UnknownContext,
    AmbiguousWorkdir,
    InvalidContextPath,
    InvalidObservationMetadata,
    ObservationValidation,
    ExactSourceRequired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PatternSourceObligations {
    pub source_coverage_complete: bool,
    pub context_complete: bool,
    pub counts: Vec<PatternObligationCount>,
    pub details: Vec<PatternSourceObligation>,
    pub details_truncated: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PatternObligationCount {
    pub kind: PatternObligationKind,
    pub count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PatternSourceObligation {
    pub span: BashSpan,
    pub kind: PatternObligationKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum PatternObligationKind {
    ProgramEffectsNotAssessed,
    ExactSourceRequired,
    Operator(BashOperatorKind),
    Assignment,
    Redirect,
    Payload,
    UnknownSource,
    SyntaxError,
    SourceGap,
    OverlappingCoverage,
    UnsupportedSyntax,
    AmbiguousRedirect,
    Context(BashContextIssue),
}

impl PatternCallDiagnostics {
    fn omit(&mut self, span: &BashSpan, reason: PatternOmissionReason) {
        *self.omission_counts.entry(reason.clone()).or_default() += 1;
        if self.omissions.len() < MAX_PATTERN_DIAGNOSTIC_DETAILS {
            self.omissions.push(PatternCommandOmission {
                span: span.clone(),
                reason,
            });
        } else {
            self.omissions_truncated += 1;
        }
    }
}

impl PatternSourceObligations {
    fn record(&mut self, span: &BashSpan, kind: PatternObligationKind) {
        if let Some(count) = self.counts.iter_mut().find(|count| count.kind == kind) {
            count.count += 1;
        } else {
            self.counts.push(PatternObligationCount {
                kind: kind.clone(),
                count: 1,
            });
        }
        if self.details.len() < MAX_PATTERN_DIAGNOSTIC_DETAILS {
            self.details.push(PatternSourceObligation {
                span: span.clone(),
                kind,
            });
        } else {
            self.details_truncated += 1;
        }
    }
}

pub fn analyze_pattern_calls(
    source: &str,
    initial_workdir: &Path,
    project: &Path,
    assumptions: BashContextAssumptions,
) -> Result<PatternCallAnalysis, BashParseError> {
    let program = parse_bash(source)?;
    let contexts = program.command_contexts_with_assumptions(initial_workdir, assumptions);
    let facts = shell_facts(&program, &contexts);
    let facts_by_span: BTreeMap<_, _> = facts
        .commands
        .iter()
        .map(|command| ((command.span.start, command.span.end), command))
        .collect();
    let mut spans: Vec<_> = program
        .commands()
        .map(|(id, _)| &program.nodes()[id.0].span)
        .collect();
    spans.sort_by_key(|span| span.start);
    let mut diagnostics = PatternCallDiagnostics {
        represented_command_count: spans.len(),
        observed_command_count: 0,
        omission_counts: BTreeMap::new(),
        omissions: Vec::new(),
        omissions_truncated: 0,
        obligations: source_obligations(&program, &contexts, facts.opaque()),
    };
    let mut observations = Vec::new();
    for span in spans {
        let result = facts_by_span
            .get(&(span.start, span.end))
            .ok_or(PatternOmissionReason::UnrepresentableScope)
            .and_then(|command| {
                checked_command_observation(
                    &program,
                    command,
                    initial_workdir,
                    project,
                    ObservationProvenance::Imported,
                )
            });
        match result {
            Ok(observation) if !facts.opaque() => observations.push(observation),
            Ok(_) => diagnostics.omit(span, PatternOmissionReason::ExactSourceRequired),
            Err(reason) => diagnostics.omit(span, reason),
        }
    }
    diagnostics.observed_command_count = observations.len();
    let requires_exact_source = facts.opaque();
    Ok(PatternCallAnalysis {
        observations,
        contexts,
        requires_exact_source,
        diagnostics,
    })
}

fn source_obligations(
    program: &BashProgram,
    contexts: &BashCommandContexts,
    requires_exact_source: bool,
) -> PatternSourceObligations {
    let mut obligations = PatternSourceObligations {
        source_coverage_complete: program.is_complete(),
        context_complete: contexts.complete,
        counts: Vec::new(),
        details: Vec::new(),
        details_truncated: 0,
    };
    let source_span = BashSpan {
        start: 0,
        end: program.source().len(),
    };
    obligations.record(
        &source_span,
        PatternObligationKind::ProgramEffectsNotAssessed,
    );
    if requires_exact_source {
        obligations.record(&source_span, PatternObligationKind::ExactSourceRequired);
    }
    for coverage in program.coverage() {
        let kind = match &coverage.role {
            BashCoverageKind::Word | BashCoverageKind::Trivia => continue,
            BashCoverageKind::Operator(operator) => {
                PatternObligationKind::Operator(operator.clone())
            }
            BashCoverageKind::Assignment => PatternObligationKind::Assignment,
            BashCoverageKind::Redirect => PatternObligationKind::Redirect,
            BashCoverageKind::Payload => PatternObligationKind::Payload,
            BashCoverageKind::Unknown => PatternObligationKind::UnknownSource,
        };
        obligations.record(&coverage.span, kind);
    }
    for diagnostic in program.diagnostics() {
        let kind = match &diagnostic.kind {
            BashDiagnosticKind::SyntaxError => PatternObligationKind::SyntaxError,
            BashDiagnosticKind::SourceGap => PatternObligationKind::SourceGap,
            BashDiagnosticKind::OverlappingCoverage => PatternObligationKind::OverlappingCoverage,
            BashDiagnosticKind::UnsupportedSyntax(_) => PatternObligationKind::UnsupportedSyntax,
            BashDiagnosticKind::AmbiguousRedirect => PatternObligationKind::AmbiguousRedirect,
        };
        obligations.record(&diagnostic.span, kind);
    }
    for diagnostic in &contexts.diagnostics {
        let span = program
            .nodes()
            .get(diagnostic.node.0)
            .map_or(&source_span, |node| &node.span);
        obligations.record(
            span,
            PatternObligationKind::Context(diagnostic.issue.clone()),
        );
    }
    obligations
}

pub(crate) struct CommandFacts<'a> {
    pub span: &'a BashSpan,
    pub command: &'a BashCommand,
    pub context: Option<&'a BashCommandContext>,
    pub scope: ShellCommandScope,
}

pub(crate) struct ShellFacts<'a> {
    pub commands: Vec<CommandFacts<'a>>,
    /// Part of the line is beyond the analysis: syntax it does not model, a
    /// command it cannot scope or place, a wrapper, or a word, assignment or
    /// redirect it cannot read.
    pub unaccounted: bool,
    /// A redirect opens a file, which no command scope shows.
    pub redirects_to_files: bool,
}

impl ShellFacts<'_> {
    /// Whether the reviewed commands describe less than the line does.
    pub fn opaque(&self) -> bool {
        self.unaccounted || self.redirects_to_files
    }
}

/// What a redirect reaches beyond the descriptors the command already holds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RedirectEffect<'a> {
    /// Duplicates or closes a descriptor, or opens the null device.
    Inert,
    Reads(&'a str),
    Writes(&'a str),
    /// A descriptor or target the source does not spell out.
    Unknown,
}

pub(crate) fn shell_facts<'a>(
    program: &'a BashProgram,
    contexts: &'a BashCommandContexts,
) -> ShellFacts<'a> {
    let contexts_by_node: BTreeMap<_, _> = contexts
        .commands
        .iter()
        .map(|context| (context.command.0, context))
        .collect();
    let mut commands = Vec::new();
    let mut unaccounted = !program.is_complete() || !contexts.complete;
    let mut redirects_to_files = false;
    for (id, command) in program.commands() {
        let Some(scope) = command_scope(program, command) else {
            unaccounted = true;
            continue;
        };
        let context = contexts_by_node.get(&id.0).copied();
        unaccounted |= context.is_none_or(|context| {
            !context.complete
                || !matches!(&context.incoming, BashCwdSet::Known(paths) if !paths.is_empty())
        }) || command.static_argv().is_none()
            || !command.assignments.is_empty()
            || OPAQUE_WRAPPERS.contains(&scope.executable.as_str());
        for redirect in &command.redirects {
            match redirect_effect(program, redirect) {
                RedirectEffect::Inert => {}
                RedirectEffect::Reads(_) | RedirectEffect::Writes(_) => redirects_to_files = true,
                RedirectEffect::Unknown => unaccounted = true,
            }
        }
        commands.push(CommandFacts {
            span: &program.nodes()[id.0].span,
            command,
            context,
            scope,
        });
    }
    commands.sort_by_key(|command| command.span.start);
    unaccounted |= commands.is_empty() || commands.len() != contexts_by_node.len();
    ShellFacts {
        commands,
        unaccounted,
        redirects_to_files,
    }
}

fn command_scope(program: &BashProgram, command: &BashCommand) -> Option<ShellCommandScope> {
    let first = command.words.first()?;
    let raw_executable = program.text(&first.span)?;
    let executable = first.literal.as_deref().unwrap_or(raw_executable);
    let basename = Path::new(executable).file_name()?.to_str()?;
    let words = command
        .words
        .iter()
        .map(|word| program.text(&word.span))
        .collect::<Option<Vec<_>>>()?;
    let source = words.join(" ");
    let normalized = if words.len() == 1 {
        basename.to_owned()
    } else {
        format!("{basename} {}", words[1..].join(" "))
    };
    Some(ShellCommandScope {
        start_byte: first.span.start,
        source,
        normalized,
        permission: format!("{basename} *"),
        executable: basename.into(),
        arguments: Some(
            command
                .words
                .iter()
                .skip(1)
                .map(|word| {
                    word.literal
                        .clone()
                        .map_or(ShellWord::Undecodable, ShellWord::Literal)
                })
                .collect(),
        ),
    })
}

pub(crate) fn redirect_effect<'a>(
    program: &BashProgram,
    redirect: &'a BashRedirect,
) -> RedirectEffect<'a> {
    let numbered = redirect
        .descriptor
        .as_ref()
        .is_none_or(|span| program.text(span).is_some_and(decimal));
    if !numbered {
        return RedirectEffect::Unknown;
    }
    let Some(target) = &redirect.target else {
        return match redirect.kind {
            BashRedirectKind::CloseInput | BashRedirectKind::CloseOutput => RedirectEffect::Inert,
            _ => RedirectEffect::Unknown,
        };
    };
    let Some(target) = target.literal.as_deref() else {
        return RedirectEffect::Unknown;
    };
    match redirect.kind {
        BashRedirectKind::CloseInput | BashRedirectKind::CloseOutput => RedirectEffect::Unknown,
        BashRedirectKind::DuplicateInput | BashRedirectKind::DuplicateOutput => {
            if target == "-" || decimal(target.strip_suffix('-').unwrap_or(target)) {
                RedirectEffect::Inert
            } else {
                RedirectEffect::Unknown
            }
        }
        _ if target == NULL_DEVICE => RedirectEffect::Inert,
        BashRedirectKind::Read => RedirectEffect::Reads(target),
        BashRedirectKind::Write
        | BashRedirectKind::Append
        | BashRedirectKind::WriteBoth
        | BashRedirectKind::AppendBoth
        | BashRedirectKind::Clobber => RedirectEffect::Writes(target),
    }
}

pub(crate) fn singleton_workdir<'a>(facts: &CommandFacts<'a>) -> Option<&'a Path> {
    checked_singleton_workdir(facts).ok()
}

fn checked_singleton_workdir<'a>(
    facts: &CommandFacts<'a>,
) -> Result<&'a Path, PatternOmissionReason> {
    let context = facts
        .context
        .filter(|context| context.complete)
        .ok_or(PatternOmissionReason::UnknownContext)?;
    match &context.incoming {
        BashCwdSet::Known(paths) if paths.len() == 1 => Ok(paths[0].as_path()),
        BashCwdSet::Known(paths) if !paths.is_empty() => {
            Err(PatternOmissionReason::AmbiguousWorkdir)
        }
        _ => Err(PatternOmissionReason::UnknownContext),
    }
}

pub(crate) fn command_observation(
    program: &BashProgram,
    facts: &CommandFacts<'_>,
    initial_workdir: &Path,
    project: &Path,
    provenance: ObservationProvenance,
) -> Option<CommandObservation> {
    checked_command_observation(program, facts, initial_workdir, project, provenance).ok()
}

fn checked_command_observation(
    program: &BashProgram,
    facts: &CommandFacts<'_>,
    initial_workdir: &Path,
    project: &Path,
    provenance: ObservationProvenance,
) -> Result<CommandObservation, PatternOmissionReason> {
    if !program.is_complete() {
        return Err(PatternOmissionReason::IncompleteSource);
    }
    if !facts.command.assignments.is_empty() {
        return Err(PatternOmissionReason::ShellAssignments);
    }
    if !facts.command.redirects.is_empty() {
        return Err(PatternOmissionReason::ShellRedirects);
    }
    let argv = facts
        .command
        .static_argv()
        .ok_or(PatternOmissionReason::NonStaticArguments)?;
    check_executable(&argv, &facts.scope)?;
    if argv.iter().any(|value| sensitive(value)) {
        return Err(PatternOmissionReason::SensitiveArguments);
    }
    let workdir = checked_singleton_workdir(facts)?;
    if !workdir.is_absolute() || !initial_workdir.is_absolute() || !project.is_absolute() {
        return Err(PatternOmissionReason::InvalidContextPath);
    }
    let initial_workdir = initial_workdir
        .to_str()
        .ok_or(PatternOmissionReason::InvalidContextPath)?;
    let project = project
        .to_str()
        .ok_or(PatternOmissionReason::InvalidContextPath)?;
    let workdir = workdir
        .to_str()
        .ok_or(PatternOmissionReason::InvalidContextPath)?;
    let input_hash = canonical_json_sha256(&json!({
        "command": program.source(),
        "workdir": initial_workdir,
        "project": project,
    }));
    let identity = canonical_json_sha256(&json!({
        "input": input_hash,
        "span": facts.span,
        "workdir": workdir,
    }));
    let source_kind = if provenance == ObservationProvenance::Native {
        PREPARED_SOURCE
    } else {
        IMPORTED_SOURCE
    };
    let observation = CommandObservation {
        version: OBSERVATION_SCHEMA_VERSION,
        roles: argument_roles(&argv),
        context: PatternContext {
            tool_identity: TOOL_IDENTITY.into(),
            executable_identity: argv
                .first()
                .ok_or(PatternOmissionReason::ExecutableIdentity)?
                .to_string(),
            effective_workdir: workdir.into(),
            path_binding: project.into(),
            analysis_version: format!(
                "{ANALYSIS_VERSION}/{}:{}",
                program.analysis_version(),
                program.grammar_version()
            ),
        },
        argv: argv.into_iter().map(str::to_owned).collect(),
        verification: ObservationVerification {
            complete_command: true,
            static_argv: true,
            context_verified: true,
            sensitivity_checked: true,
            shell_effects: ShellEffectStatus::Absent,
        },
        source: ObservationSource {
            source_identity: format!("{source_kind}:{input_hash}"),
            observation_id: identity,
            input_hash,
            session_id: source_kind.into(),
            timestamp_ms: u64::try_from(jiff::Timestamp::now().as_millisecond())
                .map_err(|_| PatternOmissionReason::InvalidObservationMetadata)?,
            outcome: if provenance == ObservationProvenance::Native {
                InvocationOutcome::Requested
            } else {
                InvocationOutcome::Unknown
            },
            provenance,
        },
    };
    observation
        .validate()
        .map_err(|_| PatternOmissionReason::ObservationValidation)?;
    Ok(observation)
}

fn check_executable(argv: &[&str], scope: &ShellCommandScope) -> Result<(), PatternOmissionReason> {
    let executable = argv
        .first()
        .ok_or(PatternOmissionReason::ExecutableIdentity)?;
    let executable = executable.to_ascii_lowercase();
    let name = executable.strip_suffix(".exe").unwrap_or(&executable);
    if scope.source != scope.normalized {
        return Err(PatternOmissionReason::ExecutableIdentity);
    }
    if OPAQUE_WRAPPERS.contains(&name) {
        return Err(PatternOmissionReason::OpaqueWrapper);
    }
    if PAYLOAD_EXECUTABLES.contains(&name)
        || VERSIONED_INTERPRETERS.iter().any(|interpreter| {
            name.strip_prefix(*interpreter).is_some_and(|version| {
                version.is_empty()
                    || version.starts_with(|character: char| character.is_ascii_digit())
                        && version
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || byte == b'.')
            })
        })
        || Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| SCRIPT_EXTENSIONS.contains(&extension))
    {
        return Err(PatternOmissionReason::InterpretedExecutable);
    }
    if argv
        .iter()
        .skip(1)
        .any(|argument| payload_argument(argument))
    {
        return Err(PatternOmissionReason::PotentialPayloadArgument);
    }
    Ok(())
}

fn payload_argument(argument: &str) -> bool {
    if argument.starts_with('@') {
        return true;
    }
    if argument == "--" {
        return false;
    }
    if argument.starts_with("--") {
        let flag = argument
            .split_once('=')
            .map_or(argument, |(flag, _)| flag)
            .to_ascii_lowercase();
        return PAYLOAD_FLAGS
            .iter()
            .any(|payload| payload.starts_with(flag.as_str()));
    }
    argument.strip_prefix('-').is_some_and(|cluster| {
        cluster
            .bytes()
            .any(|byte| matches!(byte.to_ascii_lowercase(), b'c' | b'e' | b'f' | b'z'))
    })
}

fn argument_roles(argv: &[&str]) -> Vec<ArgumentRole> {
    let mut leading_operations = true;
    let mut terminated = false;
    argv.iter()
        .enumerate()
        .map(|(index, value)| {
            if index == 0 {
                ArgumentRole::Executable
            } else if terminated {
                ArgumentRole::Unknown
            } else if *value == "--" {
                leading_operations = false;
                terminated = true;
                ArgumentRole::OptionTerminator
            } else if value.starts_with('-') {
                leading_operations = false;
                ArgumentRole::Flag
            } else if leading_operations && !value.is_empty() {
                ArgumentRole::Operation
            } else {
                leading_operations = false;
                ArgumentRole::Unknown
            }
        })
        .collect()
}

fn sensitive(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    let compact = lowercase.replace(['-', '_'], "");
    value.chars().any(char::is_control)
        || value.contains("://")
        || value.contains('@')
        || value.starts_with("AKIA")
        || value.starts_with("eyJ")
        || SENSITIVE_MARKERS
            .iter()
            .any(|marker| lowercase.contains(marker) || compact.contains(marker))
        || SENSITIVE_FLAGS.iter().any(|flag| value.starts_with(flag))
        || compact
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|part| matches!(part, "key" | "sig" | "accesskey" | "sessionid"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use caudra_agent::permissions::pattern_recognition::{
        InvocationOutcome, ObservationProvenance,
    };
    use caudra_storage::permission_patterns::ArgumentRole;
    use test_case::test_case;

    use super::{
        BashContextAssumptions, BashContextIssue, BashOperatorKind, BashSpan,
        MAX_PATTERN_DIAGNOSTIC_DETAILS, PatternCommandOmission, PatternObligationKind,
        PatternOmissionReason, PatternSourceObligations, analyze_pattern_calls, argument_roles,
        command_observation, parse_bash, shell_facts,
    };

    const HISTORICAL_ROOT: &str = "/nonexistent/historical-project";
    const ANALYSIS_EXPECTED: &str = "analysis";
    const SERIALIZATION_EXPECTED: &str = "serialized diagnostics";
    const COMMAND_SPAN_EXPECTED: &str = "fixture command span";
    const STATIC_COMMAND: &str = "cargo check -p core";
    const PAYLOAD_COMMAND: &str = "python3 -c 'print(1)'";
    const DIAGNOSTIC_OVERFLOW: usize = 3;

    fn assumptions() -> BashContextAssumptions {
        BashContextAssumptions {
            startup_preserves_cwd: true,
            no_aliases_functions_or_command_not_found_hook: true,
            no_traps: true,
            default_shell_options: true,
            standard_builtins: true,
            directory_variables_are_standard: true,
            cdpath_empty: true,
            lastpipe_disabled: true,
            logical_pwd_matches_initial: true,
        }
    }

    #[test_case(false; "historical_assumptions_unknown")]
    #[test_case(true; "historical_assumptions_explicit")]
    fn historical_context_is_explicit_and_never_native(declared: bool) {
        let root = Path::new(HISTORICAL_ROOT);
        let result = analyze_pattern_calls(
            "cd crate && cargo check -p core",
            root,
            root,
            if declared {
                assumptions()
            } else {
                BashContextAssumptions::default()
            },
        )
        .expect("analysis");
        assert_eq!(result.contexts.complete, declared);
        assert_eq!(result.requires_exact_source, !declared);
        assert_eq!(result.observations.len(), usize::from(declared));
        if declared {
            let observation = &result.observations[0];
            observation.validate().expect("valid observation");
            assert_eq!(
                observation.source.provenance,
                ObservationProvenance::Imported
            );
            assert_eq!(observation.source.outcome, InvocationOutcome::Unknown);
            assert_eq!(
                Path::new(&observation.context.effective_workdir),
                root.join("crate")
            );
        }
    }

    #[test_case("cargo check -p core", 1; "plain_static_argv")]
    #[test_case("cd left && cargo check -p core", 1; "success_singleton")]
    #[test_case("cd left || cargo check -p core", 1; "failure_singleton")]
    #[test_case("cd left; cargo check -p core", 0; "multiple_possible_workdirs")]
    #[test_case("cd left || cd right; cargo check -p core", 0; "branches_not_fake_observations")]
    #[test_case("cargo check -p core >out", 0; "file_redirect")]
    #[test_case("cargo check -p core 2>&1", 0; "descriptor_is_not_absent")]
    #[test_case("cargo check -p core 2>/dev/null", 0; "discard_is_not_absent")]
    #[test_case("MODE=debug cargo check -p core", 0; "environment_attribute")]
    #[test_case("cargo check -p $(echo core)", 0; "substitution")]
    #[test_case("cargo check -p '*'", 1; "quoted_literal")]
    #[test_case("cargo check -p *", 0; "unquoted_expansion")]
    #[test_case("cargo check -p core --token=redacted", 0; "secret_flag")]
    #[test_case("cargo check -p core --api-key redacted", 0; "secret_separate_value")]
    #[test_case("cargo check -p core -HAuthorization:redacted", 0; "attached_credential_flag")]
    #[test_case("cargo check -p core --value ghp_redacted", 0; "credential_format")]
    #[test_case("cargo check -p core --value 'Basic YWJjZA=='", 0; "basic_credential")]
    #[test_case("cargo check -p core --value '{\"key\":\"abcd\"}'", 0; "quoted_credential_key")]
    #[test_case("cargo check -p core --value https://user:pass@example.test", 0; "url_credentials")]
    #[test_case("cargo check -p core --value '-----BEGIN PRIVATE KEY-----'", 0; "private_key")]
    #[test_case("cargo check -p core --config 'build.rustc-wrapper=helper'", 0; "cargo_helper")]
    #[test_case("git -c alias.status=helper status", 0; "git_helper")]
    #[test_case("rg --pre helper needle", 0; "search_helper")]
    #[test_case("python3 -c 'print(1)'", 0; "interpreter_payload")]
    #[test_case("bash -c 'cargo check'", 0; "shell_payload")]
    #[test_case("./cargo check -p core", 0; "executable_script")]
    #[test_case("/usr/bin/cargo check -p core", 0; "unverified_executable_path")]
    #[test_case("custom-tool --value literal", 1; "generic_cli_execution_observation")]
    #[test_case("git push origin main", 1; "execution_is_not_read_only_certification")]
    #[test_case("cargo custom-operation --name literal", 1; "no_positive_operation_allowlist")]
    #[test_case("python3.13 --version", 0; "versioned_interpreter")]
    #[test_case("python3.exe --version", 0; "interpreter_executable_suffix")]
    #[test_case("node --version", 0; "known_javascript_interpreter")]
    #[test_case("perl -v", 0; "known_perl_interpreter")]
    #[test_case("awk 'BEGIN { print 1 }'", 0; "embedded_awk_language")]
    #[test_case("sed -n '1p' file", 0; "read_only_sed_is_still_a_language")]
    #[test_case("ssh host true", 0; "remote_shell_payload")]
    #[test_case("task.py --name alpha", 0; "bare_script_extension")]
    #[test_case("novelctl inspect -c body", 0; "code_flag")]
    #[test_case("novelctl inspect -ebody", 0; "attached_expression_flag")]
    #[test_case("novelctl inspect -f code", 0; "script_file_flag")]
    #[test_case("novelctl inspect -xc body", 0; "clustered_code_flag")]
    #[test_case("novelctl inspect --eval=body", 0; "long_eval_flag")]
    #[test_case("novelctl inspect --script body", 0; "long_script_flag")]
    #[test_case("novelctl inspect --command body", 0; "long_command_flag")]
    #[test_case("novelctl inspect --config settings", 0; "configuration_flag")]
    #[test_case("novelctl inspect --conf settings", 0; "abbreviated_configuration_flag")]
    #[test_case("novelctl inspect @arguments.rsp", 0; "response_file")]
    #[test_case("novelctl inspect --name 'opaque language text'", 1; "unknown_callee_language_is_not_claimed_safe")]
    #[test_case("printf -v CDPATH /elsewhere; cargo check", 0; "shell_state")]
    fn observations_require_static_effect_absent_nonsensitive_facts(source: &str, count: usize) {
        let root = Path::new(HISTORICAL_ROOT);
        let result =
            analyze_pattern_calls(source, root, root, assumptions()).expect(ANALYSIS_EXPECTED);
        assert_eq!(result.observations.len(), count);
        let program = parse_bash(source).expect(ANALYSIS_EXPECTED);
        let contexts = program.command_contexts_with_assumptions(root, assumptions());
        let facts = shell_facts(&program, &contexts);
        let native_count = facts
            .commands
            .iter()
            .filter(|_| !facts.opaque())
            .filter_map(|command| {
                command_observation(&program, command, root, root, ObservationProvenance::Native)
            })
            .count();
        assert_eq!(native_count, count);
        assert_eq!(result.diagnostics.observed_command_count, count);
        assert_eq!(
            result.diagnostics.represented_command_count,
            count + result.diagnostics.omission_counts.values().sum::<usize>()
        );
    }

    fn obligation_count(
        obligations: &PatternSourceObligations,
        kind: PatternObligationKind,
    ) -> usize {
        obligations
            .counts
            .iter()
            .find(|entry| entry.kind == kind)
            .map_or(0, |entry| entry.count)
    }

    #[test_case("'cargo' check -p core", PatternOmissionReason::ExecutableIdentity, false; "quoted_executable")]
    #[test_case("/usr/bin/cargo check -p core", PatternOmissionReason::ExecutableIdentity, false; "absolute_executable")]
    #[test_case("./cargo check -p core", PatternOmissionReason::ExecutableIdentity, false; "relative_executable")]
    #[test_case("rg -n authentication source", PatternOmissionReason::SensitiveArguments, false; "sensitive_search_term")]
    #[test_case("cargo check --token=redacted", PatternOmissionReason::SensitiveArguments, false; "sensitive_argument")]
    #[test_case("git -C /fixture status", PatternOmissionReason::PotentialPayloadArgument, false; "conservative_payload_flag")]
    #[test_case("rg -i -e fixture source", PatternOmissionReason::PotentialPayloadArgument, false; "search_expression")]
    #[test_case(PAYLOAD_COMMAND, PatternOmissionReason::InterpretedExecutable, false; "interpreter")]
    #[test_case("cargo check -p core 2>/dev/null", PatternOmissionReason::ShellRedirects, false; "null_redirect")]
    #[test_case("cargo check -p core >out", PatternOmissionReason::ShellRedirects, true; "file_redirect")]
    #[test_case("cargo check -p core 2>&1", PatternOmissionReason::ShellRedirects, false; "descriptor_redirect")]
    #[test_case("MODE=debug cargo check -p core", PatternOmissionReason::ShellAssignments, true; "assignment")]
    #[test_case("cargo check -p *", PatternOmissionReason::NonStaticArguments, true; "expansion")]
    #[test_case("bash -c 'cargo check'", PatternOmissionReason::OpaqueWrapper, true; "opaque_wrapper")]
    #[test_case("''", PatternOmissionReason::UnrepresentableScope, true; "missing_scope")]
    fn omissions_identify_single_command_rejections(
        source: &str,
        reason: PatternOmissionReason,
        requires_exact_source: bool,
    ) {
        let root = Path::new(HISTORICAL_ROOT);
        let result =
            analyze_pattern_calls(source, root, root, assumptions()).expect(ANALYSIS_EXPECTED);
        assert!(result.observations.is_empty());
        assert_eq!(result.requires_exact_source, requires_exact_source);
        let diagnostics = result.diagnostics;
        assert_eq!(diagnostics.represented_command_count, 1);
        assert_eq!(diagnostics.observed_command_count, 0);
        assert_eq!(
            diagnostics.omissions,
            vec![PatternCommandOmission {
                span: BashSpan {
                    start: 0,
                    end: source.len(),
                },
                reason: reason.clone(),
            }]
        );
        assert_eq!(
            diagnostics.omission_counts,
            BTreeMap::from([(reason.clone(), 1)])
        );
        assert_eq!(diagnostics.omissions_truncated, 0);
        if reason == PatternOmissionReason::ShellRedirects {
            assert_eq!(
                obligation_count(&diagnostics.obligations, PatternObligationKind::Redirect),
                1
            );
        }
        if reason == PatternOmissionReason::ShellAssignments {
            assert_eq!(
                obligation_count(&diagnostics.obligations, PatternObligationKind::Assignment),
                1
            );
        }
    }

    #[test_case("cargo check -p core && python3 -c 'print(1)'", 1, false, vec![(PAYLOAD_COMMAND, PatternOmissionReason::InterpretedExecutable)]; "payload_sibling")]
    #[test_case("cd left || cd right; cargo check -p core", 0, false, vec![("cd left", PatternOmissionReason::InterpretedExecutable), ("cd right", PatternOmissionReason::InterpretedExecutable), (STATIC_COMMAND, PatternOmissionReason::AmbiguousWorkdir)]; "ambiguous_cwd")]
    #[test_case("cargo check -p core; git status >out", 0, true, vec![(STATIC_COMMAND, PatternOmissionReason::ExactSourceRequired), ("git status >out", PatternOmissionReason::ShellRedirects)]; "opaque_sibling_suppresses_observations")]
    fn omitted_scopes_are_not_hidden_by_returned_observations(
        source: &str,
        observed: usize,
        requires_exact_source: bool,
        omissions: Vec<(&str, PatternOmissionReason)>,
    ) {
        let root = Path::new(HISTORICAL_ROOT);
        let result =
            analyze_pattern_calls(source, root, root, assumptions()).expect(ANALYSIS_EXPECTED);
        assert_eq!(result.observations.len(), observed);
        assert_eq!(result.requires_exact_source, requires_exact_source);
        let diagnostics = result.diagnostics;
        assert_eq!(diagnostics.observed_command_count, observed);
        assert_eq!(
            diagnostics.represented_command_count,
            observed + omissions.len()
        );
        let omissions: Vec<_> = omissions
            .into_iter()
            .map(|(command, reason)| {
                let start = source.find(command).expect(COMMAND_SPAN_EXPECTED);
                PatternCommandOmission {
                    span: BashSpan {
                        start,
                        end: start + command.len(),
                    },
                    reason,
                }
            })
            .collect();
        assert_eq!(diagnostics.omissions, omissions);
    }

    #[test_case(STATIC_COMMAND, 1, PatternObligationKind::ProgramEffectsNotAssessed; "plain_command_still_has_unassessed_program_effects")]
    #[test_case("cargo check -p core &", 1, PatternObligationKind::Operator(BashOperatorKind::Background); "background")]
    #[test_case("cargo check -p core && git status", 2, PatternObligationKind::Operator(BashOperatorKind::And); "conditional")]
    #[test_case("cargo check -p core; git status", 2, PatternObligationKind::Operator(BashOperatorKind::Semicolon); "sequence")]
    #[test_case("cargo check -p core\ngit status", 2, PatternObligationKind::Operator(BashOperatorKind::Newline); "newline_sequence")]
    #[test_case("cargo check -p core | head -n 1", 2, PatternObligationKind::Operator(BashOperatorKind::Pipe); "pipeline")]
    #[test_case("(cargo check -p core)", 1, PatternObligationKind::Operator(BashOperatorKind::OpenSubshell); "subshell")]
    #[test_case("{ cargo check -p core; }", 1, PatternObligationKind::Operator(BashOperatorKind::OpenBrace); "brace_group")]
    fn observing_every_represented_command_does_not_discharge_source_obligations(
        source: &str,
        observed: usize,
        kind: PatternObligationKind,
    ) {
        let root = Path::new(HISTORICAL_ROOT);
        let result =
            analyze_pattern_calls(source, root, root, assumptions()).expect(ANALYSIS_EXPECTED);
        assert!(!result.requires_exact_source);
        assert_eq!(result.observations.len(), observed);
        assert_eq!(result.diagnostics.represented_command_count, observed);
        assert!(result.diagnostics.omissions.is_empty());
        let obligations = result.diagnostics.obligations;
        assert!(obligations.source_coverage_complete);
        assert!(obligations.context_complete);
        assert_eq!(obligation_count(&obligations, kind.clone()), 1);
        assert!(obligations.details.iter().any(|detail| detail.kind == kind));
        assert_eq!(
            obligation_count(
                &obligations,
                PatternObligationKind::ProgramEffectsNotAssessed
            ),
            1
        );
    }

    #[test_case(false; "undeclared_assumptions")]
    #[test_case(true; "declared_assumptions")]
    fn unknown_context_is_reported_without_exporting_paths(declared: bool) {
        let root = Path::new(HISTORICAL_ROOT);
        let result = analyze_pattern_calls(
            STATIC_COMMAND,
            root,
            root,
            if declared {
                assumptions()
            } else {
                BashContextAssumptions::default()
            },
        )
        .expect(ANALYSIS_EXPECTED);
        assert_eq!(result.requires_exact_source, !declared);
        assert_eq!(result.diagnostics.represented_command_count, 1);
        assert_eq!(
            result.diagnostics.observed_command_count,
            usize::from(declared)
        );
        assert_eq!(result.diagnostics.obligations.context_complete, declared);
        assert_eq!(
            result
                .diagnostics
                .omission_counts
                .get(&PatternOmissionReason::UnknownContext)
                .copied()
                .unwrap_or_default(),
            usize::from(!declared)
        );
        assert_eq!(
            obligation_count(
                &result.diagnostics.obligations,
                PatternObligationKind::Context(BashContextIssue::UndeclaredShellAssumptions)
            ),
            usize::from(!declared)
        );
        let serialized = serde_json::to_string(&result.diagnostics).expect(SERIALIZATION_EXPECTED);
        assert!(!serialized.contains(HISTORICAL_ROOT));
    }

    #[test_case("python3 - <<'PY'\nprint(1)\nPY\n", PatternObligationKind::SourceGap, true; "heredoc_source_gap")]
    #[test_case("python3 <<'PY'\nprint(1)\nPY\n", PatternObligationKind::UnsupportedSyntax, true; "heredoc_unsupported")]
    #[test_case("cargo check -p $(echo core)", PatternObligationKind::UnsupportedSyntax, false; "substitution")]
    fn incomplete_ir_counts_are_not_whole_source_coverage(
        source: &str,
        kind: PatternObligationKind,
        payload: bool,
    ) {
        let root = Path::new(HISTORICAL_ROOT);
        let result =
            analyze_pattern_calls(source, root, root, assumptions()).expect(ANALYSIS_EXPECTED);
        assert!(result.requires_exact_source);
        assert!(result.observations.is_empty());
        assert_eq!(result.diagnostics.represented_command_count, 0);
        assert!(result.diagnostics.omissions.is_empty());
        let obligations = result.diagnostics.obligations;
        assert!(!obligations.source_coverage_complete);
        assert!(obligation_count(&obligations, kind) > 0);
        assert!(obligation_count(&obligations, PatternObligationKind::UnknownSource) > 0);
        assert_eq!(
            obligation_count(&obligations, PatternObligationKind::Payload) > 0,
            payload
        );
    }

    #[test_case("cargo check -p alpha && python3 -c 'print(\"alpha\")'", "cargo check -p bravo && python3 -c 'print(\"bravo\")'"; "payload_and_observed_arguments")]
    #[test_case("rg -n authentication_alpha fixture_alpha", "rg -n authentication_bravo fixture_bravo"; "sensitive_arguments")]
    #[test_case("/private/alpha/cargo check", "/private/bravo/cargo check"; "executable_paths")]
    #[test_case("cargo check >alpha", "cargo check >bravo"; "redirect_targets")]
    #[test_case("python3 - <<'ALPHA'\nprint('alpha')\nALPHA\n", "python3 - <<'BRAVO'\nprint('bravo')\nBRAVO\n"; "unsupported_source_and_payload")]
    fn diagnostic_serialization_contains_no_source_literals(first: &str, second: &str) {
        let first_root = Path::new("/private/alpha");
        let second_root = Path::new("/private/bravo");
        let first = analyze_pattern_calls(first, first_root, first_root, assumptions())
            .expect(ANALYSIS_EXPECTED);
        let second = analyze_pattern_calls(second, second_root, second_root, assumptions())
            .expect(ANALYSIS_EXPECTED);
        let first = serde_json::to_string(&first.diagnostics).expect(SERIALIZATION_EXPECTED);
        let second = serde_json::to_string(&second.diagnostics).expect(SERIALIZATION_EXPECTED);
        assert_eq!(first, second);
        assert!(!first.contains("alpha"));
        assert!(!second.contains("bravo"));
    }

    #[test_case(MAX_PATTERN_DIAGNOSTIC_DETAILS + DIAGNOSTIC_OVERFLOW)]
    fn bounded_details_retain_exact_omission_and_obligation_counts(command_count: usize) {
        let source = format!("{} &", vec![PAYLOAD_COMMAND; command_count].join("; "));
        let root = Path::new(HISTORICAL_ROOT);
        let result =
            analyze_pattern_calls(&source, root, root, assumptions()).expect(ANALYSIS_EXPECTED);
        assert!(!result.requires_exact_source);
        let diagnostics = result.diagnostics;
        assert_eq!(diagnostics.represented_command_count, command_count);
        assert_eq!(diagnostics.observed_command_count, 0);
        assert_eq!(diagnostics.omissions.len(), MAX_PATTERN_DIAGNOSTIC_DETAILS);
        assert_eq!(diagnostics.omissions_truncated, DIAGNOSTIC_OVERFLOW);
        assert_eq!(
            diagnostics.omission_counts,
            BTreeMap::from([(PatternOmissionReason::InterpretedExecutable, command_count)])
        );
        let obligations = diagnostics.obligations;
        assert_eq!(obligations.details.len(), MAX_PATTERN_DIAGNOSTIC_DETAILS);
        assert!(obligations.details_truncated > 0);
        assert_eq!(
            obligations
                .counts
                .iter()
                .map(|entry| entry.count)
                .sum::<usize>(),
            obligations.details.len() + obligations.details_truncated
        );
        assert_eq!(
            obligation_count(
                &obligations,
                PatternObligationKind::Operator(BashOperatorKind::Semicolon)
            ),
            command_count - 1
        );
        assert_eq!(
            obligation_count(
                &obligations,
                PatternObligationKind::Operator(BashOperatorKind::Background)
            ),
            1
        );
    }

    #[test_case(&["cargo", "check", "-p", "core"], vec![ArgumentRole::Executable, ArgumentRole::Operation, ArgumentRole::Flag, ArgumentRole::Unknown]; "cargo_package_is_not_guessed")]
    #[test_case(&["git", "branch", "topic"], vec![ArgumentRole::Executable, ArgumentRole::Operation, ArgumentRole::Operation]; "leading_positions_are_fixed")]
    #[test_case(&["cargo", "test", "--", "--option"], vec![ArgumentRole::Executable, ArgumentRole::Operation, ArgumentRole::OptionTerminator, ArgumentRole::Unknown]; "terminator_does_not_prove_cli_data_semantics")]
    #[test_case(&["tool", "", "value"], vec![ArgumentRole::Executable, ArgumentRole::Unknown, ArgumentRole::Unknown]; "empty_argument_is_not_an_operation")]
    fn roles_pin_operations_and_flags_without_guessing_data(
        argv: &[&str],
        roles: Vec<ArgumentRole>,
    ) {
        assert_eq!(argument_roles(argv), roles);
    }
}
