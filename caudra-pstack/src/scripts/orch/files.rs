//! The store's plain files, read and written byte for byte as upstream's `store.ts` does, so a
//! store either one wrote reads in the other.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::compat::{js_trim, locale_compare, random_uuid, safe_integer};
use super::model::{
    Frontier, FrontierPr, Gate, InboxPointer, LedgerEntry, OrchError, OrchResult, PrState,
    StandingLine, Unit, parse_verdict,
};

pub const UNITS_FILE: &str = "units.tsv";
pub const LEDGER_FILE: &str = "ledger.tsv";
pub const INBOX_DIR: &str = "inbox";
pub const GATES_FILE: &str = "gates.md";
pub const PREFERENCES_FILE: &str = "preferences.md";
pub const FRONTIER_FILE: &str = "frontier.json";
pub const STATUS_FILE: &str = "status.md";
pub const UNIT_HEADER: &str = "id\ttrack\tstate\tbranch\tpr\tsha\tbrief";
pub const LEDGER_HEADER: &str = "pr\tsha\tverdict\tevidence\tverifier\tts";
pub const EMPTY_FRONTIER: &str = "{}\n";
const UNIT_WIDTH: usize = 7;
const LEDGER_WIDTH: usize = 6;
const POINTER_WIDTH: usize = 5;
const POINTER_SUFFIX: &str = ".tsv";
const GATES_PREFIX: &str = "# Gates\n\n## ";
const GATE_SEPARATOR: &str = "\n\n## ";
const FORMULA_PREFIXES: [char; 4] = ['=', '+', '-', '@'];

static GATE_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^- ([^:]+): ([^\n\r\x{2028}\x{2029}]*)$").expect("literal"));
static STANDING_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([1-9][0-9]*)\. ([^\n\r\x{2028}\x{2029}]+)$").expect("literal"));

pub fn not_initialized(store: &Path) -> OrchError {
    OrchError::user(format!(
        "store is not initialized at {}; run orch init",
        store.display()
    ))
}

/// Flattens a cell onto one line and defuses spreadsheet formulas with a leading quote.
pub fn clean_cell(value: &str) -> String {
    let cleaned = value.replace(['\t', '\n', '\r'], " ");
    if cleaned.starts_with(FORMULA_PREFIXES) {
        format!("'{cleaned}")
    } else {
        cleaned
    }
}

pub fn required_cell(value: &str, label: &str) -> OrchResult<String> {
    let cleaned = clean_cell(value);
    if js_trim(&cleaned).is_empty() {
        return Err(OrchError::user(format!("{label} must not be empty")));
    }
    Ok(cleaned)
}

pub fn required_line(value: &str, label: &str) -> OrchResult<String> {
    let flattened = value.replace(['\n', '\r'], " ");
    let cleaned = js_trim(&flattened);
    if cleaned.is_empty() {
        return Err(OrchError::user(format!("{label} must not be empty")));
    }
    Ok(cleaned.to_owned())
}

pub fn exists(path: &Path) -> OrchResult<bool> {
    Ok(path.try_exists()?)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Writes through an exclusively created sibling and renames it over `path`.
pub fn atomic_write(path: &Path, contents: &str) -> OrchResult<()> {
    let directory = path.parent().unwrap_or(path);
    let temporary = directory.join(format!(
        ".{}.{}.{}.tmp",
        file_name(path),
        std::process::id(),
        random_uuid()
    ));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .and_then(|()| fs::rename(&temporary, path));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    Ok(written?)
}

pub fn write_if_missing(path: &Path, contents: &str) -> OrchResult<()> {
    if !exists(path)? {
        atomic_write(path, contents)?;
    }
    Ok(())
}

pub fn read_text(path: &Path) -> OrchResult<String> {
    Ok(String::from_utf8_lossy(&fs::read(path)?).into_owned())
}

fn required_file(path: &Path) -> OrchResult<String> {
    read_text(path).map_err(|error| match error {
        OrchError::Io(io) if io.kind() == ErrorKind::NotFound => {
            not_initialized(path.parent().unwrap_or(path))
        }
        other => other,
    })
}

fn read_tsv(path: &Path, header: &str, width: usize) -> OrchResult<Vec<Vec<String>>> {
    let raw = required_file(path)?.replace('\r', "");
    let mut lines = raw.split('\n');
    if lines.next() != Some(header) {
        return Err(OrchError::user(format!(
            "{} has an invalid header",
            file_name(path)
        )));
    }
    lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let cells: Vec<String> = line.split('\t').map(str::to_owned).collect();
            if cells.len() != width {
                return Err(OrchError::user(format!(
                    "{} has a malformed row",
                    file_name(path)
                )));
            }
            Ok(cells)
        })
        .collect()
}

fn write_tsv<'a, const N: usize>(
    path: &Path,
    header: &str,
    rows: impl Iterator<Item = [&'a str; N]>,
) -> OrchResult<()> {
    let body = rows
        .map(|row| row.map(clean_cell).join("\t"))
        .collect::<Vec<_>>()
        .join("\n");
    let terminator = if body.is_empty() { "" } else { "\n" };
    atomic_write(path, &format!("{header}\n{body}{terminator}"))
}

pub fn read_units(store: &Path) -> OrchResult<Vec<Unit>> {
    Ok(read_tsv(&store.join(UNITS_FILE), UNIT_HEADER, UNIT_WIDTH)?
        .into_iter()
        .map(|row| {
            let [id, track, state, branch, pr, sha, brief] =
                <[String; UNIT_WIDTH]>::try_from(row).unwrap_or_default();
            Unit {
                id,
                track,
                state,
                branch,
                pr,
                sha,
                brief,
            }
        })
        .collect())
}

pub fn save_units(store: &Path, rows: &[Unit]) -> OrchResult<()> {
    write_tsv(
        &store.join(UNITS_FILE),
        UNIT_HEADER,
        rows.iter().map(Unit::cells),
    )
}

pub fn read_ledger(store: &Path) -> OrchResult<Vec<LedgerEntry>> {
    read_tsv(&store.join(LEDGER_FILE), LEDGER_HEADER, LEDGER_WIDTH)?
        .into_iter()
        .map(|row| {
            let [pr, sha, verdict, evidence, verifier, ts] =
                <[String; LEDGER_WIDTH]>::try_from(row).unwrap_or_default();
            let verdict = parse_verdict(&verdict).map_err(|_| {
                OrchError::user(format!("{LEDGER_FILE} has invalid verdict {verdict}"))
            })?;
            Ok(LedgerEntry {
                pr,
                sha,
                verdict,
                evidence,
                verifier,
                ts,
            })
        })
        .collect()
}

pub fn save_ledger(store: &Path, rows: &[LedgerEntry]) -> OrchResult<()> {
    write_tsv(
        &store.join(LEDGER_FILE),
        LEDGER_HEADER,
        rows.iter().map(LedgerEntry::cells),
    )
}

pub fn pointer_contents(pointer: &InboxPointer) -> String {
    format!("{}\n", pointer.cells().map(clean_cell).join("\t"))
}

pub fn read_pointers(inbox: &Path) -> OrchResult<Vec<InboxPointer>> {
    let entries = match fs::read_dir(inbox) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(not_initialized(inbox.parent().unwrap_or(inbox)));
        }
        Err(error) => return Err(error.into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file() && name.ends_with(POINTER_SUFFIX) {
            names.push(name);
        }
    }
    names.sort_by(|left, right| locale_compare(left, right));
    names
        .into_iter()
        .map(|name| {
            let text = read_text(&inbox.join(&name))?;
            let raw = text.strip_suffix('\n').map_or(text.as_str(), |line| {
                line.strip_suffix('\r').unwrap_or(line)
            });
            let malformed = || OrchError::user(format!("inbox pointer {name} is malformed"));
            if raw.contains(['\r', '\n']) {
                return Err(malformed());
            }
            let cells: Vec<&str> = raw.split('\t').collect();
            let [ts, agent, unit, status, report] =
                <[&str; POINTER_WIDTH]>::try_from(cells).map_err(|_| malformed())?;
            Ok(InboxPointer {
                ts: ts.to_owned(),
                agent: agent.to_owned(),
                unit: unit.to_owned(),
                status: status.to_owned(),
                report: report.to_owned(),
            })
        })
        .collect()
}

pub fn render_gates(rows: &[Gate]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let blocks: Vec<String> = rows
        .iter()
        .map(|gate| {
            let answer = gate
                .answer
                .as_ref()
                .map(|answer| format!("\n- Answer: {answer}"))
                .unwrap_or_default();
            format!(
                "## {}\n\n- Status: {}\n- Question: {}\n- Options: {}\n- Default: {}{answer}",
                gate.id,
                gate.kind(),
                gate.question,
                gate.options,
                gate.default_answer
            )
        })
        .collect();
    format!("# Gates\n\n{}\n", blocks.join("\n\n"))
}

pub fn read_gates(store: &Path) -> OrchResult<Vec<Gate>> {
    let text = required_file(&store.join(GATES_FILE))?.replace('\r', "");
    let raw = js_trim(&text);
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let Some(body) = raw.strip_prefix(GATES_PREFIX) else {
        return Err(OrchError::user("gates.md has an invalid heading"));
    };
    let mut result: Vec<Gate> = Vec::new();
    for block in body.split(GATE_SEPARATOR) {
        let mut lines = block.split('\n').filter(|line| !line.is_empty());
        let id = lines.next().unwrap_or_default().to_owned();
        let malformed = || OrchError::user(format!("gates.md has a malformed gate {id}"));
        let mut fields: Vec<(&str, &str)> = Vec::new();
        for line in lines {
            let captures = GATE_FIELD.captures(line).ok_or_else(malformed)?;
            let (name, value) = (
                captures.get(1).map_or("", |found| found.as_str()),
                captures.get(2).map_or("", |found| found.as_str()),
            );
            fields.retain(|(existing, _)| *existing != name);
            fields.push((name, value));
        }
        let field = |name: &str| {
            fields
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        };
        let (Some(question), Some(options), Some(default_answer)) =
            (field("Question"), field("Options"), field("Default"))
        else {
            return Err(malformed());
        };
        if id.is_empty() {
            return Err(malformed());
        }
        let status = field("Status");
        let answer = match (status.as_deref(), field("Answer")) {
            (Some("open"), _) => None,
            (Some("resolved"), Some(answer)) => Some(answer),
            _ => {
                return Err(OrchError::user(format!(
                    "gates.md has invalid status {}",
                    status.unwrap_or_default()
                )));
            }
        };
        result.push(Gate {
            id,
            question,
            options,
            default_answer,
            answer,
        });
    }
    let mut ids: Vec<&str> = result.iter().map(|gate| gate.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != result.len() {
        return Err(OrchError::user("gates.md has duplicate gate ids"));
    }
    Ok(result)
}

fn shape_error() -> OrchError {
    OrchError::user("frontier.json has an invalid shape")
}

fn frontier_row(row: &Value) -> Option<FrontierPr> {
    let row = row.as_object()?;
    let pr = safe_integer(row.get("pr")?).filter(|pr| *pr >= 1)?;
    let branches = row
        .get("branches")?
        .as_str()
        .filter(|text| !text.is_empty())?;
    Some(FrontierPr {
        pr: pr.unsigned_abs(),
        branches: branches.to_owned(),
        sha: row.get("sha")?.as_str()?.to_owned(),
        state: PrState::parse(row.get("state")?.as_str()?)?,
    })
}

fn parse_frontier_object(value: &Map<String, Value>) -> OrchResult<Frontier> {
    if value.is_empty() {
        return Ok(Frontier {
            generation: 0,
            prs: Vec::new(),
            lowest_unmerged: None,
        });
    }
    let generation = value
        .get("generation")
        .and_then(safe_integer)
        .filter(|generation| *generation >= 0)
        .ok_or_else(shape_error)?;
    let prs = value
        .get("prs")
        .and_then(Value::as_array)
        .ok_or_else(shape_error)?;
    let lowest_unmerged = match value.get("lowestUnmerged") {
        Some(Value::Null) => None,
        Some(number) => Some(safe_integer(number).ok_or_else(shape_error)?),
        None => return Err(shape_error()),
    };
    let prs = prs
        .iter()
        .map(|row| {
            frontier_row(row).ok_or_else(|| OrchError::user("frontier.json has an invalid PR row"))
        })
        .collect::<OrchResult<_>>()?;
    Ok(Frontier {
        generation: generation.unsigned_abs(),
        prs,
        lowest_unmerged,
    })
}

pub fn parse_frontier(raw: &str) -> OrchResult<Frontier> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| OrchError::user("frontier.json is not valid JSON"))?;
    let Value::Object(object) = value else {
        return Err(OrchError::user("frontier.json must contain an object"));
    };
    parse_frontier_object(&object)
}

pub fn read_frontier(store: &Path) -> OrchResult<Frontier> {
    parse_frontier(&required_file(&store.join(FRONTIER_FILE))?)
}

pub fn frontier_contents(frontier: &Frontier) -> OrchResult<String> {
    let json = serde_json::to_string_pretty(frontier).map_err(std::io::Error::other)?;
    Ok(format!("{json}\n"))
}

pub fn read_standing(store: &Path) -> OrchResult<Vec<StandingLine>> {
    let raw = required_file(&store.join(PREFERENCES_FILE))?.replace('\r', "");
    if js_trim(&raw).is_empty() {
        return Ok(Vec::new());
    }
    let mut result: Vec<StandingLine> = Vec::new();
    for value in raw.split('\n').filter(|line| !line.is_empty()) {
        let captures = STANDING_LINE.captures(value);
        let number = captures
            .as_ref()
            .and_then(|found| found.get(1)?.as_str().parse::<usize>().ok());
        let line = captures.as_ref().and_then(|found| found.get(2));
        match (number, line) {
            (Some(number), Some(line)) if number == result.len() + 1 => {
                result.push(StandingLine {
                    number,
                    line: line.as_str().to_owned(),
                });
            }
            _ => return Err(OrchError::user("preferences.md has malformed numbering")),
        }
    }
    Ok(result)
}

pub fn standing_contents(rows: &[StandingLine]) -> String {
    let lines: Vec<String> = rows
        .iter()
        .map(|row| format!("{}. {}", row.number, row.line))
        .collect();
    format!("{}\n", lines.join("\n"))
}
