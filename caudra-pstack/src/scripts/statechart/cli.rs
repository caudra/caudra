//! The command line of upstream's `statechart.mjs`, with `compare-charts.mjs` as a third command:
//! `from-spec` checks a spec and draws its charts, `from-chart` writes a spec from an agreed chart
//! and checks that the spec draws it back, and `compare` compares two charts. tla-checker stands
//! in for TLC, mappings are Rhai, and the charts stay Mermaid source: there is no `--png`.

use std::collections::HashMap;
use std::fs;
use std::iter;
use std::path::Path;

use serde_json::{Map, Value as Json, json};
use thiserror::Error;

use super::chart::{ChartError, Charts, DrawOptions, draw_with};
use super::check::{Checked, ModelInput, check, file_name, split_rules};
use super::from_chart::{ChartFile, chart_to_spec, compare_charts, drawn_back, module_name};
use super::mapping::{Mapping, scaffold};
use super::publish::{CHECKS, Inputs, Named, WorkFolder, refuse_foreign};
use super::second::{Role, SecondModel, listed_breaks};
use super::text::{join_and, rule_sentences};
use super::uncited::uncited_conditions;
use crate::host::{Host, Output};

const FROM_SPEC: &str = "from-spec";
const FROM_CHART: &str = "from-chart";
const COMPARE: &str = "compare";
const HELP_FLAGS: [&str; 2] = ["-h", "--help"];
const MAP: &str = "--map";
const CONFIG: &str = "--config";
const FIXED: &str = "--fixed";
const FIXED_CONFIG: &str = "--fixed-config";
const AGAINST: &str = "--against";
const AGAINST_CONFIG: &str = "--against-config";
const LABEL: &str = "--label";
const EXPECT_BROKEN: &str = "--expect-broken";
const TITLE: &str = "--title";
const OUT: &str = "--out";
const MODULE: &str = "--module";
const PNG: &str = "--png";
const FROM_SPEC_FLAGS: [&str; 10] = [
    MAP,
    CONFIG,
    FIXED,
    FIXED_CONFIG,
    AGAINST,
    AGAINST_CONFIG,
    LABEL,
    EXPECT_BROKEN,
    TITLE,
    OUT,
];
const FROM_CHART_FLAGS: [&str; 4] = [MODULE, EXPECT_BROKEN, TITLE, OUT];
const SPEC_EXTENSION: &str = ".tla";
const CFG_EXTENSION: &str = ".cfg";
const MAP_EXTENSION: &str = ".map.rhai";
const JAVASCRIPT_EXTENSIONS: [&str; 3] = [".mjs", ".js", ".cjs"];
const OUT_SUFFIX: &str = "-statechart";
const FIXED_NAME: &str = "Fixed";
const AGREED_NAME: &str = "Agreed";
const CHART_FILE: &str = "chart.mmd";
const STORIES_FILE: &str = "stories.md";
const SUMMARY_FILE: &str = "summary.json";
const REGIONS_PROOF_FILE: &str = "regions-proof.mmd";
const REGIONS_ALL_FILE: &str = "regions-all.mmd";
const UNCITED_KEY: &str = "uncited";
const CONDITION_WIDTH: usize = 90;
const ELLIPSIS: &str = "...";
/// The exit code of a run that cannot finish.
const CANNOT_FINISH: i32 = 2;
/// The exit code of a run whose rules or chart are not as expected.
const NOT_AS_EXPECTED: i32 = 1;

const USAGE_FROM_SPEC: &str = "statechart from-spec SPEC.tla [--config FILE] [--fixed FIXED.tla [--fixed-config FILE] | --against AGREED.tla [--against-config FILE]] [--label TEXT] [--map FILE] [--expect-broken RULE]... [--title TEXT] [--out DIR]";
const USAGE_FROM_CHART: &str = "statechart from-chart CHART.mmd [--module NAME] [--expect-broken RULE]... [--title TEXT] [--out DIR]";
const USAGE_COMPARE: &str = "statechart compare EXPECTED.mmd ACTUAL.mmd";
const USAGE_HELP: &str = "statechart [from-spec|from-chart|compare] --help";
const CALLING: &str = r#"Each word is one string argument of pstack.statechart in the code tool, as in
pstack.statechart("from-spec", "Pay.tla", "--fixed", "PayFixed.tla"), which returns
{"code", "stdout", "stderr"}. It draws a Mermaid statechart of what an app's user sees, from the
state graph of a TLA+ spec that tla-checker explores. The help calls that user the reader."#;

const HELP_FROM_SPEC: &str = r#"from-spec SPEC.tla
  Explores the spec with tla-checker under SPEC.cfg, and checks each INVARIANT in every state it
  reaches. Each bug path stops at its first state that breaks the rule, so a rule that stays
  broken reports only the step that broke it. SPEC.cfg has SPECIFICATION and one INVARIANT line
  per rule. Each PROPERTY, a rule about whole behaviors, is checked in a run of its own, and the
  charts do not draw it. A rule listed as both INVARIANT and PROPERTY is checked once, as an
  INVARIANT. tla-checker names each step after the innermost named action that takes it, so give
  each kind of step its own named action. The command warns when steps carry the name of the
  whole next-state action. The command prints the config that it checks each model with, and
  stops when a config does not exist, with the configs beside the spec.

  --config FILE      The config for SPEC.tla. Default: SPEC.cfg next to the spec.
  --fixed FIXED.tla  The model of the open fix. It is a copy of the spec with its own file and
                     module name, such as SpecFixed.tla with MODULE SpecFixed. The command checks
                     it the same way, with FIXED.cfg, or with the config of SPEC.tla when there is
                     no FIXED.cfg. The charts then mark each bad state fixed or still wrong.
  --fixed-config FILE
                     The config for FIXED.tla.
  --against AGREED.tla
                     The spec that SPEC.tla must follow, when SPEC.tla models the code. It needs
                     its own file and module name. The command checks it with AGREED.cfg, and
                     never with the config of SPEC.tla. SPEC.tla may add variables, record fields,
                     and actions that AGREED.tla does not have. The comparison leaves them out.
                     Do not use it with --fixed.
  --against-config FILE
                     The config for AGREED.tla, when it is not AGREED.cfg. Give the config that the
                     spec's authors say the unchanged spec passes. A rule that this config does not
                     check is "not checked" in the stories.
  --label TEXT       The name of the --fixed or --against model in the charts and the stories.
                     Default: "the fix", or "the spec".
  --map FILE         The labels for each model state and step, in Rhai. Default: SPEC.map.rhai
                     next to the spec. When the file does not exist, the command writes one from
                     the spec's VARIABLES and actions to fill in, and exits 2 before it checks the
                     spec. Its comments say what each function gets. The mapping holds only
                     functions: state(v, facts) is required, and step, guard, timer, cause, group,
                     detail, reader_actions, region_names, legend, and group_colors are optional.

  With --fixed or --against, SPEC.tla (and FIXED.tla) models the code, and the command finds each
  condition of an action that no comment ties to code with a file and line, as in \* pay.ts:120.
  A condition is a conjunct of the action that changes nothing and reads a variable that the
  action does not change. A rule can hold only because the model waits for a condition that the
  code does not check, so the command prints these conditions when a rule holds.
  With --against, each story says whether a config beside AGREED.tla expects the checker to
  break the bug's rule. A config's "\* expect: RULE" line says what the check finds, and
  "\* module: NAME" names the module it checks when the folder has more than one .tla file. The
  command warns when no config beside AGREED.tla has an expect line for that module."#;

const HELP_FROM_CHART: &str = r#"from-chart CHART.mmd
  Writes NAME.tla, NAME.cfg, and NAME.map.rhai next to the chart. When NAME.tla already exists, it
  writes none of them. Then it checks the spec as from-spec does, and compares the chart that the
  spec draws with CHART.mmd.

  --module NAME      The TLA+ module name. Default: the chart's file name in CamelCase, such as
                     NotesApp for notes-app.mmd.

  The chart is a Mermaid stateDiagram-v2. Each transition label is one action. For parallel
  regions, split one top-level composite with --, and put each part in a named composite, such as
  state Connection { ... }. Each region is one variable. A step moves every region whose current
  state has a transition with the step's label, and the other regions stay where they are. Each
  rule is one comment line:
    %% never NAME: A & B -- the rule in one sentence
  Each name after the colon is a state caption or a composite, and each comes from a different
  region. A composite stands for every state in it. The command refuses final, choice, fork, and
  history states, and regions below the top level."#;

const HELP_COMPARE: &str = r#"compare EXPECTED.mmd ACTUAL.mmd
  Compares two Mermaid stateDiagram-v2 charts: reachable states, start states, and labeled
  transitions. from-chart runs it to check that the spec still draws the chart that the reader
  agreed to, and it repeats a from-spec run's regions proof:
    statechart compare OUT/checks/regions-all.mmd OUT/checks/regions-proof.mmd
  ACTUAL is a chart drawn for comparison: every label in full, no boxes, no numbered path, and no
  second model. Each chart compares as the machine that its steps reach from its start states. A
  chart with parallel regions compares as its combined machine. A step that does not change the
  state is not compared. An edge label "a or b" counts as two labels. It prints each difference
  and then "N difference(s)", or prints "same", and writes no file."#;

const HELP_COMMON: &str = r#"from-spec and from-chart
  --expect-broken RULE
                     An INVARIANT or PROPERTY that is known to break. Repeat it, or list rules
                     with commas, as in A,B. The command then exits 0 when exactly these rules
                     break, and the --fixed or --against model breaks no other rule.
  --title TEXT       The chart title. Each bug chart adds ", bug N".
  --out DIR          The output folder. Default: NAME-statechart next to the input. A run writes its
                     files in a hidden folder beside it, and then replaces the whole output folder
                     with it. When runs overlap, the output folder has the files of the run that
                     finished last. The command does not use an output folder that has files that
                     it did not write.
  -h, --help         Print this help, and exit 0.

  Both write files, so plan mode refuses them; compare and --help write nothing.

Output
  The command prints whether each rule holds, the chart's counts, and one story per bug.
  from-chart also prints the comparison. The command writes these files in the output folder:
  chart.mmd          The overview.
  bug-N.mmd          Only the steps of bug N, numbered. Bugs are numbered by fewest reader steps,
                     then fewest model steps, then rule name, then cause. Bugs that only the
                     --fixed or --against model has come last.
  bug-N-developer.mmd, bug-N-layered.mmd
                     The path of bug N through the developer's states, when the mapping defines
                     detail().
  stories.md         One entry per bug: the rule, why it breaks, the shortest path in reader
                     steps, and what the --fixed or --against model does instead.
  summary.json       The counts behind the chart. With --fixed or --against, uncited has the
                     conditions with no file and line for each code model. With --against,
                     second.listed has what each config beside AGREED.tla expects, and each bug
                     has the configs that list it.
  checks/            The config and the counterexample of each PROPERTY check. When the overview
                     has regions, from-spec also writes checks/regions-all.mmd, the regions with
                     every arrow, and checks/regions-proof.mmd, every state and step of the
                     checker's graph in one region.
  A chart is Mermaid source: show it in a ```mermaid block to see it drawn.

Reading a chart
  from-spec draws the overview as two parallel regions when every state reads "context: progress":
  the contexts on one side, the progress on the other, both at the same time. It does so only when
  that has fewer states, and when the regions combine back into exactly every state and step of
  the checker's graph. Otherwise it puts the states of one context in a box named context, and
  prints why. Regions draw only the arrows of the shortest way to each state, of each bug path,
  and of the nearest step with each label.
  An arrow from a box's border leaves from every state in the box.
  In regions, a step leaves every region that has it. "[when A]" or "[unless A]" says which states
  of the other region the step needs. A loop says the region may also stay.
  Other words in [ ] come from the mapping's guard(), and "after N s" from its timer(). The
  checker does not check them, and has no clock, so a reviewer checks them against the code.
  Gray states work. Red states break a rule. With --fixed, red states stay wrong with the fix, and
  green states are fixed by it or exist only with it. With --against, red states break a rule in
  either model, and green states exist only in the spec.
  When the mapping defines group(), the fill is the state's group, which the legend names, and
  the status is a thick red or green border on top.
  A state is bad when it breaks a rule, or when every path from the start to it passes a state
  that breaks one. Each bug path stops at the first state that breaks its rule, so a rule that
  stays broken colors only the states where it first breaks and the states after them that no
  path without a bug reaches.
  A thick blue border and numbered labels mark the bug. Mermaid state diagrams cannot color an
  edge.
  A label "with the fix, STEP" or "in the spec, STEP" is a step that only the other model takes. On
  a bug's path it leaves the state before the first step that the other model shows the reader
  differently, and ends where that model goes instead. "with the fix, STEP cannot happen" or "in
  the spec, STEP cannot happen" loops on the state where that model stops the step. Outside
  regions, two states have at most one arrow between them. Its lines are the numbered steps, then
  SPEC.tla's steps, then the other model's steps, with a blank line between each group.
  An edge lists every step that takes it, most common first. Words that the steps share at the
  start are written once, as in "you tap Pay or Back".

Regenerate
  With --expect-broken, one call rebuilds every chart and its exit code says whether anything
  changed:
    pstack.statechart("from-spec", "Code.tla", "--against", "Agreed.tla", "--expect-broken", "NoLostUpload")

Exit codes
  0  Every rule holds, or exactly the --expect-broken rules break. from-chart also draws CHART.mmd
     back unchanged. compare: the charts are the same.
  1  Another rule is broken, an expected rule holds, or the chart that the spec draws is not
     CHART.mmd. The output says which. compare: the charts differ.
  2  The command cannot finish: a bad argument, a missing file, a chart or spec that it cannot
     read, a spec the checker stops on, an output folder with files that the command did not
     write, an input file that changed during the run, or a new mapping to fill in. Exit 2 does
     not change the output folder, unless the command moved it aside and cannot put it back. Then
     the message names the folder's new path. When the command cannot put its files in the output
     folder, it keeps them in the hidden work folder, and the message names it."#;

/// Why a run stops with exit code 2, and whether it keeps the files it wrote so far.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct CannotFinish {
    pub message: String,
    pub keep_work: bool,
}

impl CannotFinish {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            keep_work: false,
        }
    }

    /// The run could not publish, so it keeps its work folder for the reader.
    pub fn kept(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            keep_work: true,
        }
    }

    pub fn with(mut self, more: &str) -> Self {
        self.message.push_str(more);
        self
    }
}

/// What a run prints, as it prints it.
#[derive(Debug, Default)]
pub struct Report {
    stdout: String,
    stderr: String,
}

impl Report {
    pub fn say(&mut self, line: &str) {
        self.stdout.push_str(line);
        self.stdout.push('\n');
    }

    pub fn warn(&mut self, line: &str) {
        self.stderr.push_str(line);
        self.stderr.push('\n');
    }

    fn output(self, code: i32) -> Output {
        Output {
            code,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    FromSpec,
    FromChart,
    Compare,
}

impl Command {
    fn parse(word: &str) -> Option<Self> {
        match word {
            FROM_SPEC => Some(Self::FromSpec),
            FROM_CHART => Some(Self::FromChart),
            COMPARE => Some(Self::Compare),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::FromSpec => FROM_SPEC,
            Self::FromChart => FROM_CHART,
            Self::Compare => COMPARE,
        }
    }

    fn flags(self) -> &'static [&'static str] {
        match self {
            Self::FromSpec => &FROM_SPEC_FLAGS,
            Self::FromChart => &FROM_CHART_FLAGS,
            Self::Compare => &[],
        }
    }

    fn usage(self) -> &'static str {
        match self {
            Self::FromSpec => USAGE_FROM_SPEC,
            Self::FromChart => USAGE_FROM_CHART,
            Self::Compare => USAGE_COMPARE,
        }
    }

    fn inputs(self) -> usize {
        match self {
            Self::FromSpec | Self::FromChart => 1,
            Self::Compare => 2,
        }
    }

    fn help(self) -> String {
        let body = match self {
            Self::FromSpec => format!("{HELP_FROM_SPEC}\n\n{HELP_COMMON}"),
            Self::FromChart => format!("{HELP_FROM_CHART}\n\n{HELP_COMMON}"),
            Self::Compare => HELP_COMPARE.to_owned(),
        };
        format!("{}\n\n{CALLING}\n\n{body}", usage(&[self.usage()]))
    }
}

fn usage(lines: &[&str]) -> String {
    format!("Usage: {}", lines.join("\n       "))
}

fn top_help() -> String {
    format!(
        "{}\n\n{CALLING}\n\n{HELP_FROM_SPEC}\n\n{HELP_FROM_CHART}\n\n{HELP_COMPARE}\n\n{HELP_COMMON}",
        usage(&[USAGE_FROM_SPEC, USAGE_FROM_CHART, USAGE_COMPARE, USAGE_HELP])
    )
}

fn more(command: Option<Command>) -> String {
    let words = command.map_or_else(String::new, |command| format!("\"{}\", ", command.name()));
    format!(
        "Run pstack.statechart({words}\"--help\") for the flags, the output, and the exit codes."
    )
}

#[derive(Debug)]
struct Args {
    command: Command,
    inputs: Vec<String>,
    values: HashMap<&'static str, String>,
    expected: Vec<String>,
}

impl Args {
    fn value(&self, flag: &str) -> Option<&str> {
        self.values.get(flag).map(String::as_str)
    }

    fn input(&self) -> &str {
        self.inputs.first().map_or("", String::as_str)
    }
}

#[derive(Debug)]
enum Parsed {
    Help(String),
    Run(Args),
}

fn parse(argv: &[String]) -> Result<Parsed, CannotFinish> {
    let Some((word, rest)) = argv.split_first() else {
        return Err(CannotFinish::new(format!(
            "{}\n{}",
            usage(&[USAGE_FROM_SPEC, USAGE_FROM_CHART, USAGE_COMPARE]),
            more(None)
        )));
    };
    if HELP_FLAGS.contains(&word.as_str()) {
        return Ok(Parsed::Help(top_help()));
    }
    let Some(command) = Command::parse(word) else {
        return Err(CannotFinish::new(format!(
            "{}\n{}",
            usage(&[USAGE_FROM_SPEC, USAGE_FROM_CHART, USAGE_COMPARE]),
            more(None)
        )));
    };
    let mut args = Args {
        command,
        inputs: Vec::new(),
        values: HashMap::new(),
        expected: Vec::new(),
    };
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if HELP_FLAGS.contains(&word.as_str()) {
            return Ok(Parsed::Help(command.help()));
        }
        if word == PNG && command != Command::Compare {
            return Err(CannotFinish::new(format!(
                "{PNG}: this command writes only the Mermaid charts; show one in a ```mermaid block to see it drawn. {}",
                more(Some(command))
            )));
        }
        if let Some(&flag) = command.flags().iter().find(|&&flag| flag == word) {
            let Some(value) = words.next() else {
                return Err(CannotFinish::new(format!(
                    "{flag} needs a value. {}",
                    more(Some(command))
                )));
            };
            if flag == EXPECT_BROKEN {
                for rule in value.split(',') {
                    if !args.expected.iter().any(|known| known == rule) {
                        args.expected.push(rule.to_owned());
                    }
                }
            } else {
                args.values.insert(flag, value.clone());
            }
        } else if !word.starts_with('-') && args.inputs.len() < command.inputs() {
            args.inputs.push(word.clone());
        } else {
            return Err(CannotFinish::new(format!(
                "unknown argument {word}. {}",
                more(Some(command))
            )));
        }
    }
    if args.inputs.len() < command.inputs() {
        return Err(CannotFinish::new(format!(
            "{}\n{}",
            usage(&[command.usage()]),
            more(Some(command))
        )));
    }
    Ok(Parsed::Run(args))
}

/// A path the call names, for the permission it asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touched {
    /// A file the call reads.
    Reads(String),
    /// A folder the call writes in.
    Writes(String),
}

/// Whether a run of `argv` writes files: `from-spec` and `from-chart` do; `compare`, help, and a
/// command line that does not parse do not.
pub fn writes(argv: &[String]) -> bool {
    matches!(parse(argv), Ok(Parsed::Run(args)) if args.command != Command::Compare)
}

/// The files a run of `argv` reads and the folders it writes in: the input's folder, where
/// `from-chart` writes the spec and `from-spec` a new mapping, and the output folder.
pub fn touched(argv: &[String]) -> Vec<Touched> {
    let Ok(Parsed::Run(args)) = parse(argv) else {
        return Vec::new();
    };
    if args.command == Command::Compare {
        return args.inputs.into_iter().map(Touched::Reads).collect();
    }
    let input = args.input();
    let folder = Path::new(input)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| ".".to_owned(), |parent| parent.display().to_string());
    let name = match args.command {
        Command::FromChart => args
            .value(MODULE)
            .map(str::to_owned)
            .or_else(|| module_name(Path::new(input)).ok()),
        Command::FromSpec | Command::Compare => Some(spec_name(input)),
    };
    let out = args
        .value(OUT)
        .map(str::to_owned)
        .or_else(|| name.map(|name| beside(input, &format!("{name}{OUT_SUFFIX}"))));
    iter::once(Touched::Writes(folder))
        .chain(out.map(Touched::Writes))
        .collect()
}

pub fn run(host: &dyn Host, argv: &[String]) -> Output {
    let mut report = Report::default();
    let result = match parse(argv) {
        Ok(Parsed::Help(help)) => {
            report.say(&help);
            Ok(0)
        }
        Ok(Parsed::Run(args)) => match args.command {
            Command::FromSpec => from_spec(host, &args, &mut report),
            Command::FromChart => from_chart(host, &args, &mut report),
            Command::Compare => compare(host, &args, &mut report),
        },
        Err(error) => Err(error),
    };
    match result {
        Ok(code) => report.output(code),
        Err(error) => {
            report.warn(&error.message);
            report.output(CANNOT_FINISH)
        }
    }
}

/// `file` in the folder of `input`, as the reader would name it.
fn beside(input: &str, file: &str) -> String {
    Path::new(input)
        .parent()
        .unwrap_or(Path::new(""))
        .join(file)
        .display()
        .to_string()
}

fn spec_name(spec: &str) -> String {
    let name = file_name(spec);
    name.strip_suffix(SPEC_EXTENSION)
        .map_or(name.clone(), str::to_owned)
}

fn cfg_of(spec: &str) -> String {
    spec.strip_suffix(SPEC_EXTENSION)
        .map_or_else(|| spec.to_owned(), |stem| format!("{stem}{CFG_EXTENSION}"))
}

fn named(host: &dyn Host, shown: &str) -> Named {
    Named {
        shown: shown.to_owned(),
        path: host.resolve(shown),
    }
}

fn read(file: &Named) -> Result<String, CannotFinish> {
    fs::read_to_string(&file.path)
        .map_err(|error| CannotFinish::new(format!("{}: {error}", file.shown)))
}

fn write(path: &Path, text: &str) -> Result<(), CannotFinish> {
    fs::write(path, text)
        .map_err(|error| CannotFinish::new(format!("cannot write {}: {error}", path.display())))
}

/// Upstream's `readModel`: the spec and its config, which must both exist.
fn read_model(
    host: &dyn Host,
    spec: &str,
    cfg: &str,
    flag: &str,
) -> Result<ModelInput, CannotFinish> {
    let spec_file = named(host, spec);
    if !spec_file.path.exists() {
        return Err(CannotFinish::new(format!("{spec} does not exist")));
    }
    let cfg_file = named(host, cfg);
    if !cfg_file.path.exists() {
        let mut beside: Vec<String> = spec_file
            .path
            .parent()
            .and_then(|folder| fs::read_dir(folder).ok())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(CFG_EXTENSION))
            .collect();
        beside.sort();
        let hint = if beside.is_empty() {
            "A config has SPECIFICATION and one INVARIANT line per rule.".to_owned()
        } else {
            format!("The configs beside it: {}.", beside.join(", "))
        };
        return Err(CannotFinish::new(format!(
            "{cfg} does not exist. Pass {flag} with the config to check {} with. {hint}",
            file_name(spec)
        )));
    }
    Ok(ModelInput {
        spec: spec.to_owned(),
        cfg: cfg.to_owned(),
        rules: split_rules(&read(&cfg_file)?),
        spec_path: spec_file.path,
        cfg_path: cfg_file.path,
    })
}

/// Upstream's `readInput`: the model, whose config must name every expected rule.
fn read_input(
    host: &dyn Host,
    spec: &str,
    cfg: &str,
    expected: &[String],
) -> Result<ModelInput, CannotFinish> {
    let model = read_model(host, spec, cfg, CONFIG)?;
    let names: Vec<String> = model.rules.names().cloned().collect();
    let unknown: Vec<&str> = expected
        .iter()
        .filter(|rule| !names.contains(rule))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let rules = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        };
        return Err(CannotFinish::new(format!(
            "{EXPECT_BROKEN} {}: not a rule in {cfg}, whose rules are {rules}.",
            unknown.join(", ")
        )));
    }
    Ok(model)
}

fn out_dir(host: &dyn Host, args: &Args, name: &str) -> Result<Named, CannotFinish> {
    let shown = args.value(OUT).map_or_else(
        || beside(args.input(), &format!("{name}{OUT_SUFFIX}")),
        str::to_owned,
    );
    let out = named(host, &shown);
    refuse_foreign(&out)?;
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecondRole {
    Fixed,
    Against,
}

struct Second {
    role: SecondRole,
    label: Option<String>,
    input: ModelInput,
}

/// Upstream's `secondCfg`: the config the second model is checked with.
fn second_cfg(host: &dyn Host, cfg: &str, args: &Args) -> Result<Option<String>, CannotFinish> {
    for (config, model) in [(FIXED_CONFIG, FIXED), (AGAINST_CONFIG, AGAINST)] {
        if args.value(config).is_some() && args.value(model).is_none() {
            return Err(CannotFinish::new(format!(
                "{config} needs {model}. {}",
                more(Some(Command::FromSpec))
            )));
        }
    }
    if let Some(fixed) = args.value(FIXED) {
        let own = cfg_of(fixed);
        return Ok(Some(args.value(FIXED_CONFIG).map_or_else(
            || {
                if host.resolve(&own).exists() {
                    own
                } else {
                    cfg.to_owned()
                }
            },
            str::to_owned,
        )));
    }
    Ok(args.value(AGAINST).map(|against| {
        args.value(AGAINST_CONFIG)
            .map_or_else(|| cfg_of(against), str::to_owned)
    }))
}

/// Upstream's `secondModel`: the `--fixed` or `--against` model, if any.
fn second_model(
    host: &dyn Host,
    args: &Args,
    cfg: Option<&str>,
) -> Result<Option<Second>, CannotFinish> {
    let label = args.value(LABEL).map(str::to_owned);
    let (role, spec, flag, suffix) = match (args.value(FIXED), args.value(AGAINST)) {
        (Some(_), Some(_)) => {
            return Err(CannotFinish::new(format!(
                "use {FIXED} or {AGAINST}, not both. {}",
                more(Some(Command::FromSpec))
            )));
        }
        (None, None) if label.is_none() => return Ok(None),
        (None, None) => {
            return Err(CannotFinish::new(format!(
                "{LABEL} names the {FIXED} or {AGAINST} model; it needs one of them. {}",
                more(Some(Command::FromSpec))
            )));
        }
        (Some(fixed), None) => (SecondRole::Fixed, fixed, FIXED_CONFIG, FIXED_NAME),
        (None, Some(against)) => (SecondRole::Against, against, AGAINST_CONFIG, AGREED_NAME),
    };
    let name = spec_name(args.input());
    if spec_name(spec) == name {
        return Err(CannotFinish::new(format!(
            "{spec} needs its own file and module name, such as {name}{suffix}{SPEC_EXTENSION}."
        )));
    }
    let cfg = cfg.unwrap_or_default();
    Ok(Some(Second {
        role,
        label,
        input: read_model(host, spec, cfg, flag)?,
    }))
}

fn refuse_javascript(map: &Named) -> Result<(), CannotFinish> {
    if JAVASCRIPT_EXTENSIONS
        .iter()
        .any(|extension| map.shown.ends_with(extension))
    {
        return Err(CannotFinish::new(format!(
            "{}: a mapping is Rhai, not JavaScript. Write it as {}, with the same functions; a new one beside the spec starts from the spec's variables and actions.",
            map.shown, MAP_EXTENSION
        )));
    }
    Ok(())
}

fn load_mapping(map: &Named) -> Result<Mapping, CannotFinish> {
    Mapping::parse(&map.shown, &read(map)?).map_err(|error| CannotFinish::new(error.to_string()))
}

fn from_spec(host: &dyn Host, args: &Args, report: &mut Report) -> Result<i32, CannotFinish> {
    let input = args.input();
    let name = spec_name(input);
    let map = named(
        host,
        &args.value(MAP).map_or_else(
            || beside(input, &format!("{name}{MAP_EXTENSION}")),
            str::to_owned,
        ),
    );
    let cfg = args
        .value(CONFIG)
        .map_or_else(|| cfg_of(input), str::to_owned);
    let other_cfg = second_cfg(host, &cfg, args)?;
    let mut inputs = Inputs::default();
    for file in [Some(input), Some(cfg.as_str()), Some(map.shown.as_str())]
        .into_iter()
        .chain([
            args.value(FIXED).or(args.value(AGAINST)),
            other_cfg.as_deref(),
        ])
        .flatten()
    {
        inputs.add(&named(host, file));
    }
    let second = second_model(host, args, other_cfg.as_deref())?;
    let model = read_input(host, input, &cfg, &args.expected)?;
    let out = out_dir(host, args, &name)?;
    refuse_javascript(&map)?;
    if !map.path.exists() {
        write(&map.path, &scaffold(&name, &read(&named(host, input))?))?;
        return Err(CannotFinish::new(format!(
            "Wrote {}. Fill in its labels, then run this command again.",
            map.shown
        )));
    }
    let mapping = load_mapping(&map)?;
    in_work_folder(&out, |work| {
        let checks = work.path().join(CHECKS);
        let now = check(host, model, &checks, &out.shown, &args.expected, report)?;
        let other = second
            .map(|second| {
                let checked = check(
                    host,
                    second.input,
                    &checks,
                    &out.shown,
                    &args.expected,
                    report,
                )?;
                Ok::<_, CannotFinish>((second.role, second.label, checked))
            })
            .transpose()?;
        let mut uncited = None;
        let role = match &other {
            Some((role, label, checked)) => {
                warn_unchecked(&now, checked, report);
                let listed = match role {
                    SecondRole::Against => {
                        let listed = listed_breaks(&checked.input.spec_path).map_err(|error| {
                            CannotFinish::new(format!("{}: {error}", checked.input.spec))
                        })?;
                        if listed.is_none() && now.any_rule_broken() {
                            warn_unlisted(checked, report);
                        }
                        listed
                    }
                    SecondRole::Fixed => None,
                };
                let code_models: Vec<&Checked> = match role {
                    SecondRole::Fixed => vec![&now, checked],
                    SecondRole::Against => vec![&now],
                };
                uncited = Some(warn_uncited(&code_models, report)?);
                Some(match role {
                    SecondRole::Fixed => Role::Fix {
                        label: label.clone(),
                    },
                    SecondRole::Against => Role::Against {
                        label: label.clone(),
                        listed,
                    },
                })
            }
            None => None,
        };
        let second = other
            .as_ref()
            .zip(role)
            .map(|((_, _, checked), role)| SecondModel {
                model: &checked.model,
                role,
            });
        let mut specs = vec![&now];
        specs.extend(other.as_ref().map(|(_, _, checked)| checked));
        let drawing = Drawing {
            mapping: &mapping,
            map: &map,
            title: args.value(TITLE),
            comparing: false,
        };
        let charts = drawing.draw(&specs, second.as_ref(), uncited, work.path())?;
        drawing.report(&charts, &out, report);
        let others: Vec<&Checked> = other.iter().map(|(_, _, checked)| checked).collect();
        let ok = rules_as_expected(&now, others.first().copied(), &args.expected, report);
        finish(work, &out, &inputs, &charts, ok, report)
    })
}

fn from_chart(host: &dyn Host, args: &Args, report: &mut Report) -> Result<i32, CannotFinish> {
    let input = args.input();
    let name = match args.value(MODULE) {
        Some(module) => module.to_owned(),
        None => {
            module_name(Path::new(input)).map_err(|error| CannotFinish::new(error.to_string()))?
        }
    };
    let chart_file = named(host, input);
    let spec = beside(input, &format!("{name}{SPEC_EXTENSION}"));
    let cfg = cfg_of(&spec);
    let map = named(host, &beside(input, &format!("{name}{MAP_EXTENSION}")));
    let mut inputs = Inputs::default();
    inputs.add(&chart_file);
    let spec_file = named(host, &spec);
    if spec_file.path.exists() {
        report.say(&format!(
            "Kept {spec}. Delete it to write the spec again from {input}."
        ));
    } else {
        let generated = chart_to_spec(input, &read(&chart_file)?, &name)
            .map_err(|error| CannotFinish::new(format!("{input}: {error}")))?;
        write(&spec_file.path, &generated.tla)?;
        write(&host.resolve(&cfg), &generated.cfg)?;
        write(&map.path, &generated.mapping)?;
        report.say(&format!(
            "Wrote {name}{SPEC_EXTENSION}, {name}{CFG_EXTENSION}, and {name}{MAP_EXTENSION} from {input}."
        ));
    }
    for file in [&spec_file, &named(host, &cfg), &map] {
        inputs.add(file);
    }
    let model = read_input(host, &spec, &cfg, &args.expected)?;
    let out = out_dir(host, args, &name)?;
    let mapping = load_mapping(&map)?;
    in_work_folder(&out, |work| {
        let checks = work.path().join(CHECKS);
        let now = check(host, model, &checks, &out.shown, &args.expected, report)?;
        let drawing = Drawing {
            mapping: &mapping,
            map: &map,
            title: args.value(TITLE),
            comparing: true,
        };
        let charts = drawing.draw(&[&now], None, None, work.path())?;
        drawing.report(&charts, &out, report);
        let drawn = format!("{}/{CHART_FILE}", out.shown);
        let compared = drawn_back(
            ChartFile {
                name: input,
                text: &read(&chart_file)?,
            },
            ChartFile {
                name: &drawn,
                text: &charts.chart,
            },
        )
        .map_err(|error| CannotFinish::new(error.to_string()))?;
        report.say(&format!("\n{}", compared.report.trim_end()));
        let ok = rules_as_expected(&now, None, &args.expected, report) && compared.same;
        finish(work, &out, &inputs, &charts, ok, report)
    })
}

fn compare(host: &dyn Host, args: &Args, report: &mut Report) -> Result<i32, CannotFinish> {
    let [expected, actual] = [0, 1].map(|index| named(host, &args.inputs[index]));
    let comparison = compare_charts(
        ChartFile {
            name: &expected.shown,
            text: &read(&expected)?,
        },
        ChartFile {
            name: &actual.shown,
            text: &read(&actual)?,
        },
    )
    .map_err(|error| CannotFinish::new(error.to_string()))?;
    report.say(comparison.report().trim_end());
    Ok(if comparison.is_same() {
        0
    } else {
        NOT_AS_EXPECTED
    })
}

/// Runs `body` in a work folder beside `out`, which it removes unless the run kept it.
fn in_work_folder(
    out: &Named,
    body: impl FnOnce(&WorkFolder) -> Result<i32, CannotFinish>,
) -> Result<i32, CannotFinish> {
    let work = WorkFolder::create(out)?;
    body(&work).map_err(|error| work.discard(error))
}

struct Drawing<'a> {
    mapping: &'a Mapping,
    map: &'a Named,
    title: Option<&'a str>,
    comparing: bool,
}

impl Drawing<'_> {
    /// Draws every chart and writes it, with the stories and the summary, in `work`.
    fn draw(
        &self,
        specs: &[&Checked],
        second: Option<&SecondModel>,
        uncited: Option<Json>,
        work: &Path,
    ) -> Result<Charts, CannotFinish> {
        let mut rules = HashMap::new();
        for checked in specs.iter().rev() {
            let text = fs::read_to_string(&checked.input.spec_path).unwrap_or_default();
            rules.extend(rule_sentences(&text));
        }
        let options = DrawOptions {
            title: self.title.map(str::to_owned),
            rules,
            comparing: self.comparing,
        };
        let mut charts = draw_with(&specs[0].model, second, self.mapping, &options).map_err(
            |error| match error {
                ChartError::Mapping(error) => CannotFinish::new(error.to_string()),
                other => CannotFinish::new(format!("{}: {other}", self.map.shown)),
            },
        )?;
        if let (Some(uncited), Some(summary)) = (uncited, charts.summary.as_object_mut()) {
            summary.insert(UNCITED_KEY.to_owned(), uncited);
        }
        write(&work.join(CHART_FILE), &charts.chart)?;
        for bug in &charts.bugs {
            write(&work.join(&bug.file), &bug.mermaid)?;
        }
        write(&work.join(STORIES_FILE), &charts.stories)?;
        let summary = serde_json::to_string_pretty(&charts.summary)
            .map_err(|error| CannotFinish::new(error.to_string()))?;
        write(&work.join(SUMMARY_FILE), &format!("{summary}\n"))?;
        for (file, text) in [
            (REGIONS_PROOF_FILE, &charts.regions_proof),
            (REGIONS_ALL_FILE, &charts.regions_all),
        ] {
            if let Some(text) = text {
                write(&work.join(CHECKS).join(file), text)?;
            }
        }
        Ok(charts)
    }

    /// What upstream prints after drawing: the counts, the regions, the status line, labels that
    /// read like code, and the stories.
    fn report(&self, charts: &Charts, out: &Named, report: &mut Report) {
        let summary = &charts.summary;
        let count = |key: &str| summary[key].as_u64().unwrap_or_default();
        report.say(&format!(
            "\nChart: {} states and {} transitions from {} model states.",
            count("chartStates"),
            count("chartTransitions"),
            count("modelStates")
        ));
        let checks = |file: &str| format!("{}/{CHECKS}/{file}", out.shown);
        let regions = &summary["regions"];
        if regions["drawn"] == Json::Bool(true) {
            let number = |value: &Json| value.as_u64().unwrap_or_default();
            report.say(&format!(
                "It has 2 parallel regions, of {} and {} states. Proof: their {} arrows together make exactly the {} states and {} labeled steps of {}, and the {} drawn arrows make only steps among them. {} has all {} arrows.",
                number(&regions["states"][0]),
                number(&regions["states"][1]),
                number(&regions["all"]),
                number(&regions["flat"]["states"]),
                number(&regions["flat"]["transitions"]),
                checks(REGIONS_PROOF_FILE),
                number(&regions["transitions"]),
                checks(REGIONS_ALL_FILE),
                number(&regions["all"]),
            ));
        } else if let Some(why) = regions["why"].as_str() {
            report.say(&format!("It has no parallel regions, because {why}."));
        }
        let status = &summary["chartStatus"];
        let [works, wrong, fixed] =
            ["works", "wrong", "fixed"].map(|key| status[key].as_u64().unwrap_or_default());
        let label = summary["second"]["label"].as_str().unwrap_or_default();
        let breaks = format!(
            "{wrong} {} a rule",
            if wrong == 1 { "breaks" } else { "break" }
        );
        report.say(&match summary["second"]["role"].as_str() {
            Some("fix") => format!(
                "{works} work today, {wrong} still wrong with {label}, {fixed} fixed or new with {label}.\n"
            ),
            Some(_) => format!("{works} work, {breaks}, {fixed} only in {label}.\n"),
            None => format!("{works} work, {breaks}.\n"),
        });
        let code_like: Vec<&str> = summary["codeLike"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .collect();
        if !code_like.is_empty() {
            let listed: Vec<String> = code_like.iter().map(|text| format!("  {text}")).collect();
            report.say(&format!(
                "These labels or causes read like code. Write them in the reader's words in {}:\n{}\n",
                self.map.shown,
                listed.join("\n")
            ));
        }
        let bugs = summary["classes"].as_array().is_some_and(|c| !c.is_empty());
        report.say(if bugs {
            charts.stories.trim_end()
        } else {
            "No state breaks a rule."
        });
    }
}

/// Upstream's `finish`: publishes the work folder and says what it wrote.
fn finish(
    work: &WorkFolder,
    out: &Named,
    inputs: &Inputs,
    charts: &Charts,
    ok: bool,
    report: &mut Report,
) -> Result<i32, CannotFinish> {
    work.publish(out, inputs)?;
    let written: Vec<&str> = [CHART_FILE]
        .into_iter()
        .chain(charts.bugs.iter().map(|bug| bug.file.as_str()))
        .chain([STORIES_FILE, SUMMARY_FILE])
        .collect();
    report.say(&format!("\nWrote {} in {}.", written.join(", "), out.shown));
    Ok(if ok { 0 } else { NOT_AS_EXPECTED })
}

/// Upstream's `rulesAsExpected`: with no expected rule, whether every rule holds in both models;
/// otherwise whether exactly the expected rules break.
fn rules_as_expected(
    now: &Checked,
    other: Option<&Checked>,
    expected: &[String],
    report: &mut Report,
) -> bool {
    if expected.is_empty() {
        return !now.any_broken() && !other.is_some_and(Checked::any_broken);
    }
    let mut unexpected: Vec<&str> = Vec::new();
    for rule in now
        .broken()
        .chain(other.into_iter().flat_map(Checked::broken))
    {
        if !expected.iter().any(|known| known == rule) && !unexpected.contains(&rule) {
            unexpected.push(rule);
        }
    }
    let broken_now: Vec<&str> = now.broken().collect();
    let holds: Vec<&str> = expected
        .iter()
        .map(String::as_str)
        .filter(|rule| !broken_now.contains(rule))
        .collect();
    if !unexpected.is_empty() {
        report.say(&format!(
            "\nBroken but not expected: {}.",
            unexpected.join(", ")
        ));
    }
    if !holds.is_empty() {
        report.say(&format!(
            "\nExpected to break but holds: {}.",
            holds.join(", ")
        ));
    }
    let as_expected = unexpected.is_empty() && holds.is_empty();
    if as_expected {
        report.say(&format!("\nBroken as expected: {}.", expected.join(", ")));
    }
    as_expected
}

fn warn_unchecked(now: &Checked, other: &Checked, report: &mut Report) {
    let unchecked: Vec<&str> = now
        .input
        .rules
        .rules
        .iter()
        .filter(|rule| !other.input.rules.rules.contains(rule))
        .map(String::as_str)
        .collect();
    if unchecked.is_empty() {
        return;
    }
    let pronoun = if unchecked.len() == 1 { "it" } else { "them" };
    report.warn(&format!(
        "{} does not check {}, so the stories cannot say whether {} keeps {pronoun}.",
        other.input.cfg,
        unchecked.join(", "),
        other.input.file_name()
    ));
}

fn warn_unlisted(other: &Checked, report: &mut Report) {
    let module = other.input.module();
    report.warn(&format!(
        "No config beside {} has an \"\\* expect:\" line for {module}, so the stories cannot say whether {module} already lists each bug. When {} is a copy, pass the spec in its own folder.",
        other.input.spec,
        other.input.file_name()
    ));
}

/// Upstream's `warnUncited`: each code model's uncited conditions, warned about when a rule
/// holds, for `summary.json`.
fn warn_uncited(models: &[&Checked], report: &mut Report) -> Result<Json, CannotFinish> {
    let mut uncited = Map::new();
    for checked in models {
        let spec = &checked.input.spec;
        let text = fs::read_to_string(&checked.input.spec_path)
            .map_err(|error| CannotFinish::new(format!("{spec}: {error}")))?;
        let found = uncited_conditions(&text);
        let held = checked.held();
        if !found.is_empty() && !held.is_empty() {
            let (verb, these) = match (held.len(), found.len()) {
                (1, 1) => ("holds", "this condition"),
                (1, _) => ("holds", "these conditions"),
                (_, 1) => ("hold", "this condition"),
                _ => ("hold", "these conditions"),
            };
            let lines: Vec<String> = found
                .iter()
                .map(|condition| {
                    format!(
                        "  {spec}:{} {}: {}",
                        condition.line,
                        condition.action,
                        shortened(&condition.condition)
                    )
                })
                .collect();
            report.warn(&format!(
                "{spec}: {} {verb}, but a rule can also hold because the model waits for a condition that the code does not check. No comment gives the file and line of the code that checks {these}:\n{}\nPut that file and line in a comment on each condition, as in \\* pay.ts:120, or remove the condition when no code checks it.",
                join_and(&held),
                lines.join("\n")
            ));
        }
        uncited.insert(checked.input.file_name(), json!(found));
    }
    Ok(Json::Object(uncited))
}

fn shortened(condition: &str) -> String {
    if condition.chars().count() <= CONDITION_WIDTH {
        return condition.to_owned();
    }
    let kept: String = condition
        .chars()
        .take(CONDITION_WIDTH - ELLIPSIS.len())
        .collect();
    format!("{kept}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::host::fake::FakeHost;
    use crate::scripts::statechart::testing::fixture;

    const SYNC_FILES: [&str; 3] = ["Sync.tla", "Sync.cfg", "Sync.map.rhai"];
    const SYNC_SPEC: &str = "Sync.tla";
    const SYNC_FIXED: &str = "SyncFixed.tla";
    const SYNC_OUT: &str = "Sync-statechart";
    const SYNC_RULE: &str = "NeverSyncedWhileOffline";
    const SYNC_MAP: &str = "Sync.map.rhai";
    const SYNC_CHART: &str = "from_chart/sync.mmd";
    const SYNC_DRAWN: &str = "gold/Sync/chart.mmd";
    const SAVE_FILES: [&str; 5] = [
        "SaveCode.tla",
        "SaveCode.cfg",
        "SaveCode.map.rhai",
        "SaveAgreed.tla",
        "SaveAgreedBug.cfg",
    ];
    const STRANGER: &str = "notes.md";
    const PUBLISHED: [&str; 5] = [
        "bug-1.mmd",
        "chart.mmd",
        "checks",
        "stories.md",
        "summary.json",
    ];

    fn project(files: &[&str]) -> (TempDir, FakeHost) {
        let dir = TempDir::new().expect("a temporary folder");
        for file in files {
            let name = Path::new(file).file_name().expect("a file name");
            fs::copy(fixture(file), dir.path().join(name)).expect("the fixture copies");
        }
        let host = FakeHost::new(dir.path());
        (dir, host)
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|&word| word.to_owned()).collect()
    }

    fn call(host: &FakeHost, words: &[&str]) -> Output {
        run(host, &argv(words))
    }

    fn entries(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder lists")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn summary(dir: &TempDir) -> Json {
        let text = fs::read_to_string(dir.path().join(SYNC_OUT).join(SUMMARY_FILE))
            .expect("the summary reads");
        serde_json::from_str(&text).expect("the summary parses")
    }

    #[test]
    fn a_broken_rule_publishes_the_charts_and_exits_not_as_expected() {
        let (dir, host) = project(&SYNC_FILES);
        let output = call(&host, &[FROM_SPEC, SYNC_SPEC]);
        assert_eq!(output.code, NOT_AS_EXPECTED, "{}", output.stderr);
        assert_eq!(entries(&dir.path().join(SYNC_OUT)), PUBLISHED);
        assert!(output.stdout.contains(SYNC_RULE), "{}", output.stdout);
        let mut left = [SYNC_FILES.as_slice(), &[SYNC_OUT]].concat();
        left.sort_unstable();
        assert_eq!(entries(dir.path()), left, "no work folder is left behind");
    }

    #[test]
    fn an_expected_broken_rule_exits_ok() {
        let (_dir, host) = project(&SYNC_FILES);
        let output = call(&host, &[FROM_SPEC, SYNC_SPEC, EXPECT_BROKEN, SYNC_RULE]);
        assert_eq!(output.code, 0, "{}\n{}", output.stdout, output.stderr);
    }

    #[test]
    fn a_second_run_replaces_the_output_folder() {
        let (dir, host) = project(&SYNC_FILES);
        call(&host, &[FROM_SPEC, SYNC_SPEC]);
        let output = call(&host, &[FROM_SPEC, SYNC_SPEC]);
        assert_eq!(output.code, NOT_AS_EXPECTED, "{}", output.stderr);
        assert_eq!(entries(&dir.path().join(SYNC_OUT)), PUBLISHED);
    }

    #[test]
    fn an_output_folder_with_a_foreign_file_is_refused() {
        let (dir, host) = project(&SYNC_FILES);
        let out = dir.path().join(SYNC_OUT);
        fs::create_dir(&out).expect("the output folder");
        fs::write(out.join(STRANGER), "").expect("a file of the user's");
        let output = call(&host, &[FROM_SPEC, SYNC_SPEC]);
        assert_eq!(output.code, CANNOT_FINISH);
        assert!(
            output
                .stderr
                .contains(&format!("did not write: {STRANGER}")),
            "{}",
            output.stderr
        );
        assert_eq!(entries(&out), [STRANGER]);
    }

    #[test]
    fn a_missing_mapping_is_scaffolded_and_the_run_stops() {
        let (dir, host) = project(&SYNC_FILES[..2]);
        let output = call(&host, &[FROM_SPEC, SYNC_SPEC]);
        assert_eq!(output.code, CANNOT_FINISH);
        assert!(
            output.stderr.contains(&format!("Wrote {SYNC_MAP}")),
            "{}",
            output.stderr
        );
        assert!(dir.path().join(SYNC_MAP).exists());
        assert!(!dir.path().join(SYNC_OUT).exists());
    }

    #[test]
    fn a_fix_is_drawn_as_the_second_model() {
        let (dir, host) = project(&[&SYNC_FILES[..], &[SYNC_FIXED]].concat());
        let output = call(&host, &[FROM_SPEC, SYNC_SPEC, FIXED, SYNC_FIXED]);
        assert_eq!(output.code, NOT_AS_EXPECTED, "{}", output.stderr);
        assert_eq!(summary(&dir)["second"]["role"], "fix");
        assert!(summary(&dir).get(UNCITED_KEY).is_some());
    }

    #[test]
    fn the_agreed_spec_is_drawn_against_the_code() {
        let (dir, host) = project(&SAVE_FILES);
        let output = call(
            &host,
            &[
                FROM_SPEC,
                "SaveCode.tla",
                AGAINST,
                "SaveAgreed.tla",
                AGAINST_CONFIG,
                "SaveAgreedBug.cfg",
            ],
        );
        let text = fs::read_to_string(dir.path().join("SaveCode-statechart").join(SUMMARY_FILE))
            .expect("the summary reads");
        let summary: Json = serde_json::from_str(&text).expect("the summary parses");
        assert_eq!(
            summary["second"]["role"], "against",
            "{}\n{}",
            output.stdout, output.stderr
        );
        assert!(summary.get(UNCITED_KEY).is_some());
    }

    #[test]
    fn an_agreed_chart_is_drawn_back() {
        let (dir, host) = project(&[SYNC_CHART]);
        let output = call(&host, &[FROM_CHART, "sync.mmd"]);
        assert_eq!(
            output.code, NOT_AS_EXPECTED,
            "{}\n{}",
            output.stdout, output.stderr
        );
        assert!(dir.path().join(SYNC_SPEC).exists());
        assert_eq!(entries(&dir.path().join(SYNC_OUT)), PUBLISHED);
    }

    #[test_case(&[COMPARE, "sync.mmd", CHART_FILE], 0, ""; "the chart drawn back matches")]
    #[test_case(&[COMPARE, CHART_FILE, "sync.mmd"], CANNOT_FINISH, "sync.mmd: it has boxes; draw ACTUAL with --flat"; "an agreed chart with boxes is not drawn for comparison")]
    #[test_case(&[COMPARE, "sync.mmd"], CANNOT_FINISH, USAGE_COMPARE; "compare needs two charts")]
    fn compare_exits_with_the_verdict(words: &[&str], code: i32, message: &str) {
        let (_dir, host) = project(&[SYNC_CHART, SYNC_DRAWN]);
        let output = call(&host, words);
        assert_eq!(output.code, code, "{}", output.stderr);
        assert!(output.stderr.contains(message), "{}", output.stderr);
    }

    #[test_case(&[FROM_SPEC, SYNC_SPEC, PNG], "--png: this command writes only the Mermaid charts"; "png is refused")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, MAP, "Sync.map.mjs"], "Sync.map.mjs: a mapping is Rhai, not JavaScript"; "a javascript mapping is refused")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, FIXED_CONFIG, "Sync.cfg"], "--fixed-config needs --fixed"; "a fixed config needs the fix")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, AGAINST_CONFIG, "Sync.cfg"], "--against-config needs --against"; "an against config needs the agreed spec")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, FIXED, SYNC_FIXED, AGAINST, SYNC_FIXED], "use --fixed or --against, not both"; "fixed and against are exclusive")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, LABEL, "the fix"], "--label names the --fixed or --against model"; "a label needs a second model")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, FIXED, SYNC_SPEC], "needs its own file and module name, such as SyncFixed.tla"; "the fix needs its own module")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, EXPECT_BROKEN, "Nope"], "--expect-broken Nope: not a rule in Sync.cfg"; "an expected rule must exist")]
    #[test_case(&[FROM_SPEC, "Missing.tla"], "Missing.tla does not exist"; "the spec must exist")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, OUT], "--out needs a value"; "a flag needs its value")]
    #[test_case(&[FROM_SPEC, SYNC_SPEC, "--frobnicate"], "unknown argument --frobnicate"; "an unknown flag is refused")]
    fn a_bad_command_line_cannot_finish(words: &[&str], message: &str) {
        let (dir, host) = project(&SYNC_FILES);
        let output = call(&host, words);
        assert_eq!(output.code, CANNOT_FINISH);
        assert!(output.stderr.contains(message), "{}", output.stderr);
        assert!(!dir.path().join(SYNC_OUT).exists());
        assert!(!dir.path().join("Sync.map.mjs").exists());
    }

    #[test_case(&[FROM_SPEC, SYNC_SPEC], true; "from spec writes")]
    #[test_case(&[FROM_CHART, "sync.mmd"], true; "from chart writes")]
    #[test_case(&[COMPARE, "a.mmd", "b.mmd"], false; "compare reads")]
    #[test_case(&["--help"], false; "help reads")]
    #[test_case(&[FROM_SPEC, "--help"], false; "command help reads")]
    #[test_case(&[FROM_SPEC], false; "a command line that does not parse runs nothing")]
    fn only_drawing_runs_write(words: &[&str], expected: bool) {
        assert_eq!(writes(&argv(words)), expected);
    }

    #[test_case(&[FROM_SPEC, "specs/Pay.tla"], &[Touched::Writes("specs".into()), Touched::Writes("specs/Pay-statechart".into())]; "from spec writes beside the spec")]
    #[test_case(&[FROM_SPEC, "Pay.tla", OUT, "/tmp/out"], &[Touched::Writes(".".into()), Touched::Writes("/tmp/out".into())]; "out names the output folder")]
    #[test_case(&[FROM_CHART, "charts/sync.mmd"], &[Touched::Writes("charts".into()), Touched::Writes("charts/Sync-statechart".into())]; "from chart names its module")]
    #[test_case(&[COMPARE, "a.mmd", "b.mmd"], &[Touched::Reads("a.mmd".into()), Touched::Reads("b.mmd".into())]; "compare reads both charts")]
    fn calls_name_what_they_touch(words: &[&str], expected: &[Touched]) {
        assert_eq!(touched(&argv(words)), expected);
    }
}
