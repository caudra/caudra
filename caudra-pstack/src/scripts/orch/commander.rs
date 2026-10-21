//! The slice of commander.js 14 that upstream's command tree uses, reproduced in its parsing
//! order, error texts, suggestions, and help layout so the port prints what upstream prints.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::host::Output;

const HELP_FLAGS: [&str; 2] = ["-h", "--help"];
const HELP_TERM: &str = "-h, --help";
const HELP_DESCRIPTION: &str = "display help for command";
const LITERAL: &str = "--";
const MAX_EDIT_DISTANCE: usize = 3;
const MIN_SIMILARITY: f64 = 0.4;

static NEGATIVE_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-[0-9]*\.?[0-9]+(e[+-]?[0-9]+)?$").expect("literal"));

/// Parses a value or explains, as commander's `InvalidArgumentError`, why it cannot.
pub type Parser = fn(&str) -> Result<Value, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Flag,
    Text(String),
    Integer(u64),
    Integers(Vec<u64>),
}

pub struct Argument {
    pub name: &'static str,
    /// Shown under `Arguments:` when any argument of the command has one.
    pub description: &'static str,
    pub parser: Option<Parser>,
}

pub struct Opt {
    /// As declared, such as `--store <dir>`; a `<value>` makes it take one.
    pub flags: &'static str,
    /// The help text with commander's `(default: ...)` style suffixes already rendered.
    pub description: &'static str,
    pub mandatory: bool,
    pub env: Option<&'static str>,
    pub parser: Option<Parser>,
}

impl Opt {
    pub fn long(&self) -> &'static str {
        self.flags.split(' ').next().unwrap_or(self.flags)
    }

    fn takes_value(&self) -> bool {
        self.flags.contains('<')
    }
}

pub struct Command<A: 'static> {
    pub name: &'static str,
    pub description: &'static str,
    pub usage: Option<&'static str>,
    pub arguments: &'static [Argument],
    pub options: &'static [Opt],
    pub commands: &'static [Command<A>],
    pub action: A,
}

/// The command a parse settled on, its processed arguments, and every option value set along
/// the way, keyed by long flag. No two commands on a path share a flag.
pub struct Invocation<A> {
    pub action: A,
    pub args: Vec<Value>,
    pub options: HashMap<&'static str, Value>,
}

impl<A> Invocation<A> {
    pub fn flag(&self, long: &str) -> bool {
        self.options.contains_key(long)
    }

    pub fn text(&self, long: &str) -> Option<&str> {
        match self.options.get(long) {
            Some(Value::Text(text)) => Some(text),
            _ => None,
        }
    }

    pub fn integer(&self, long: &str) -> Option<u64> {
        match self.options.get(long) {
            Some(Value::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    pub fn integers(&self, long: &str) -> Option<&[u64]> {
        match self.options.get(long) {
            Some(Value::Integers(values)) => Some(values),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Source {
    Cli,
    Env(&'static str),
}

struct Parse<'p, A: 'static> {
    path: Vec<&'static Command<A>>,
    options: HashMap<&'static str, Value>,
    env: &'p dyn Fn(&str) -> Option<String>,
}

impl<A: Copy> Parse<'_, A> {
    fn command(&self) -> &'static Command<A> {
        self.path[self.path.len() - 1]
    }

    fn error(&self, message: &str) -> Output {
        Output::failure(1, format!("{message}\n\n{}", help(&self.path)))
    }

    fn set(
        &mut self,
        option: &'static Opt,
        raw: Option<String>,
        source: Source,
    ) -> Result<(), Output> {
        let value = match (raw, option.parser) {
            (None, _) => Value::Flag,
            (Some(raw), Some(parser)) => parser(&raw).map_err(|reason| {
                let origin = match source {
                    Source::Cli => format!("argument '{raw}'"),
                    Source::Env(var) => format!("value '{raw}' from env '{var}'"),
                };
                self.error(&format!(
                    "error: option '{}' {origin} is invalid. {reason}",
                    option.flags
                ))
            })?,
            (Some(raw), None) => Value::Text(raw),
        };
        self.options.insert(option.long(), value);
        Ok(())
    }

    fn find(&self, flag: &str) -> Option<&'static Opt> {
        self.command()
            .options
            .iter()
            .find(|option| option.long() == flag)
    }

    /// Splits `args` into operands and the unknown rest, taking this command's options.
    fn parse_options(&mut self, args: Vec<String>) -> Result<(Vec<String>, Vec<String>), Output> {
        let mut operands = Vec::new();
        let mut unknown = Vec::new();
        let mut to_unknown = false;
        let mut args = args.into_iter();
        let leaf = self.command().commands.is_empty();
        while let Some(arg) = args.next() {
            if arg == LITERAL {
                if to_unknown {
                    unknown.push(arg);
                    unknown.extend(args);
                } else {
                    operands.extend(args);
                }
                break;
            }
            let maybe_option = arg.len() > 1 && arg.starts_with('-');
            if maybe_option && let Some(option) = self.find(&arg) {
                let value = if option.takes_value() {
                    let Some(value) = args.next() else {
                        return Err(self.error(&format!(
                            "error: option '{}' argument missing",
                            option.flags
                        )));
                    };
                    Some(value)
                } else {
                    None
                };
                self.set(option, value, Source::Cli)?;
                continue;
            }
            if arg.starts_with("--")
                && let Some((flag, value)) = arg.split_once('=')
                && flag.len() > LITERAL.len()
                && let Some(option) = self.find(flag).filter(|option| option.takes_value())
            {
                self.set(option, Some(value.to_owned()), Source::Cli)?;
                continue;
            }
            if maybe_option && !(leaf && NEGATIVE_NUMBER.is_match(&arg)) {
                to_unknown = true;
            }
            if to_unknown {
                unknown.push(arg);
            } else {
                operands.push(arg);
            }
        }
        Ok((operands, unknown))
    }

    fn apply_env(&mut self) -> Result<(), Output> {
        for option in self.command().options {
            let Some(var) = option.env else { continue };
            if self.options.contains_key(option.long()) {
                continue;
            }
            if let Some(value) = (self.env)(var) {
                let raw = option.takes_value().then_some(value);
                self.set(option, raw, Source::Env(var))?;
            }
        }
        Ok(())
    }

    fn unknown_option(&self, flag: &str) -> Output {
        let suggestion = if flag.starts_with("--") {
            let candidates: Vec<&str> = self
                .path
                .iter()
                .rev()
                .flat_map(|command| command.options.iter().map(Opt::long).chain([HELP_FLAGS[1]]))
                .collect();
            suggest_similar(flag, &candidates)
        } else {
            String::new()
        };
        self.error(&format!("error: unknown option '{flag}'{suggestion}"))
    }

    fn process_arguments(&self, args: &[String]) -> Result<Vec<Value>, Output> {
        let command = self.command();
        if let Some(missing) = command.arguments.get(args.len()) {
            return Err(self.error(&format!(
                "error: missing required argument '{}'",
                missing.name
            )));
        }
        if args.len() > command.arguments.len() {
            let expected = command.arguments.len();
            let plural = if expected == 1 { "" } else { "s" };
            let subcommand = if self.path.len() > 1 {
                format!(" for '{}'", command.name)
            } else {
                String::new()
            };
            return Err(self.error(&format!(
                "error: too many arguments{subcommand}. Expected {expected} argument{plural} but got {}.",
                args.len()
            )));
        }
        command
            .arguments
            .iter()
            .zip(args)
            .map(|(argument, raw)| match argument.parser {
                Some(parser) => parser(raw).map_err(|reason| {
                    self.error(&format!(
                        "error: command-argument value '{raw}' is invalid for argument '{}'. {reason}",
                        argument.name
                    ))
                }),
                None => Ok(Value::Text(raw.clone())),
            })
            .collect()
    }
}

/// Parses `argv` against `root`, or returns what commander prints and exits with instead:
/// help with exit 0, or a parse error followed by the failing command's help with exit 1.
pub fn parse<A: Copy>(
    root: &'static Command<A>,
    argv: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Invocation<A>, Output> {
    let mut parse = Parse {
        path: vec![root],
        options: HashMap::new(),
        env,
    };
    let mut operands: Vec<String> = Vec::new();
    let mut unknown = argv.to_vec();
    loop {
        let (parsed_operands, parsed_unknown) = parse.parse_options(unknown)?;
        parse.apply_env()?;
        operands.extend(parsed_operands);
        unknown = parsed_unknown;
        let subcommand = operands.first().and_then(|name| {
            parse
                .command()
                .commands
                .iter()
                .find(|command| command.name == name)
        });
        if let Some(subcommand) = subcommand {
            operands.remove(0);
            parse.path.push(subcommand);
            continue;
        }
        if unknown.iter().any(|arg| HELP_FLAGS.contains(&arg.as_str())) {
            return Err(Output::success(help(&parse.path)));
        }
        for command in parse.path.iter().rev() {
            if let Some(option) = command
                .options
                .iter()
                .find(|option| option.mandatory && !parse.options.contains_key(option.long()))
            {
                return Err(parse.error(&format!(
                    "error: required option '{}' not specified",
                    option.flags
                )));
            }
        }
        if let Some(flag) = unknown.first() {
            return Err(parse.unknown_option(flag));
        }
        let args = parse.process_arguments(&operands)?;
        return Ok(Invocation {
            action: parse.command().action,
            args,
            options: parse.options,
        });
    }
}

fn subcommand_term<A>(command: &Command<A>) -> String {
    let mut term = command.name.to_owned();
    if !command.options.is_empty() {
        term.push_str(" [options]");
    }
    for argument in command.arguments {
        term.push_str(&format!(" <{}>", argument.name));
    }
    term
}

fn default_usage<A>(command: &Command<A>) -> String {
    let mut usage = "[options]".to_owned();
    if !command.commands.is_empty() {
        usage.push_str(" [command]");
    }
    for argument in command.arguments {
        usage.push_str(&format!(" <{}>", argument.name));
    }
    usage
}

/// The help commander prints for the last command of `path`.
pub fn help<A>(path: &[&Command<A>]) -> String {
    let Some(command) = path.last() else {
        return String::new();
    };
    let names: Vec<&str> = path.iter().map(|command| command.name).collect();
    let usage = command
        .usage
        .map_or_else(|| default_usage(command), str::to_owned);
    let arguments: Vec<(String, &str)> = if command
        .arguments
        .iter()
        .any(|argument| !argument.description.is_empty())
    {
        command
            .arguments
            .iter()
            .map(|argument| (argument.name.to_owned(), argument.description))
            .collect()
    } else {
        Vec::new()
    };
    let options: Vec<(String, &str)> = command
        .options
        .iter()
        .map(|option| (option.flags.to_owned(), option.description))
        .chain([(HELP_TERM.to_owned(), HELP_DESCRIPTION)])
        .collect();
    let commands: Vec<(String, &str)> = command
        .commands
        .iter()
        .map(|command| (subcommand_term(command), command.description))
        .collect();
    let width = arguments
        .iter()
        .chain(&options)
        .chain(&commands)
        .map(|(term, _)| term.chars().count())
        .max()
        .unwrap_or_default();
    let mut lines = vec![format!("Usage: {} {usage}", names.join(" ")), String::new()];
    if !command.description.is_empty() {
        lines.extend([command.description.to_owned(), String::new()]);
    }
    for (heading, items) in [
        ("Arguments:", arguments),
        ("Options:", options),
        ("Commands:", commands),
    ] {
        if items.is_empty() {
            continue;
        }
        lines.push(heading.to_owned());
        lines.extend(
            items
                .into_iter()
                .map(|(term, description)| format!("  {term:width$}  {description}")),
        );
        lines.push(String::new());
    }
    lines.join("\n")
}

/// Optimal string alignment distance, as commander's `editDistance`.
fn edit_distance(left: &[char], right: &[char]) -> usize {
    if left.len().abs_diff(right.len()) > MAX_EDIT_DISTANCE {
        return left.len().max(right.len());
    }
    let mut distance = vec![vec![0; right.len() + 1]; left.len() + 1];
    for (i, row) in distance.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in distance[0].iter_mut().enumerate() {
        *cell = j;
    }
    for j in 1..=right.len() {
        for i in 1..=left.len() {
            let cost = usize::from(left[i - 1] != right[j - 1]);
            let mut best = (distance[i - 1][j] + 1)
                .min(distance[i][j - 1] + 1)
                .min(distance[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                best = best.min(distance[i - 2][j - 2] + 1);
            }
            distance[i][j] = best;
        }
    }
    distance[left.len()][right.len()]
}

/// Commander's `(Did you mean ...?)` hint for a mistyped long flag.
fn suggest_similar(flag: &str, candidates: &[&str]) -> String {
    let word: Vec<char> = flag.strip_prefix(LITERAL).unwrap_or(flag).chars().collect();
    let mut unique: Vec<&str> = Vec::new();
    for candidate in candidates {
        if !unique.contains(candidate) {
            unique.push(candidate);
        }
    }
    let mut similar: Vec<&str> = Vec::new();
    let mut best_distance = MAX_EDIT_DISTANCE;
    for candidate in unique {
        let name: Vec<char> = candidate.chars().skip(2).collect();
        if name.len() <= 1 {
            continue;
        }
        let distance = edit_distance(&word, &name);
        let length = word.len().max(name.len());
        let similarity = (length as f64 - distance as f64) / length as f64;
        if similarity > MIN_SIMILARITY {
            if distance < best_distance {
                best_distance = distance;
                similar = vec![candidate];
            } else if distance == best_distance {
                similar.push(candidate);
            }
        }
    }
    similar.sort_unstable();
    match similar.as_slice() {
        [] => String::new(),
        [only] => format!("\n(Did you mean {only}?)"),
        many => format!("\n(Did you mean one of {}?)", many.join(", ")),
    }
}
