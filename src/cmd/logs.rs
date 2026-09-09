use std::io::{IsTerminal, Write};
use std::thread;
use std::time::Duration;

use caudra_storage::log::record::{Entry, Filter, Level, Record};
use caudra_storage::log::tail::{Line, LogTail};
use caudra_storage::log::{self, DEFAULT_MAX_FILES};
use color_eyre::Result;
use color_eyre::eyre::eyre;

use crate::cli::LogLevel;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const NO_LOG_DIR: &str = "no log directory on this platform";
const FIELD_GAP: &str = " ";
const KV_SEPARATOR: &str = "=";
const COLUMN_GAP: &str = "  ";
const LEVEL_WIDTH: usize = 5;
const RESET: &str = "\u{1b}[0m";
const DIM: &str = "\u{1b}[2m";

/// Prints the rotating log the daemon and the TUI share. Never installs a
/// subscriber: a reader that logs would append to the file it is printing.
pub fn run(follow: bool, level: LogLevel, lines: usize, json: bool) -> Result<()> {
    let dir = log::tail_dir().ok_or_else(|| eyre!(NO_LOG_DIR))?;
    let filter = Filter::new(level.into(), "");
    let mut tail = LogTail::open(&dir, DEFAULT_MAX_FILES, lines.max(1))?;
    tail.tail(&filter)?;

    let mut out = std::io::stdout().lock();
    let colour = out.is_terminal();
    for line in tail.window() {
        write_line(&mut out, line, json, colour)?;
    }
    out.flush()?;

    if !follow {
        return Ok(());
    }
    loop {
        let added = tail.poll(&filter)?;
        if added > 0 {
            let window = tail.window();
            for line in window.iter().skip(window.len() - added) {
                write_line(&mut out, line, json, colour)?;
            }
            out.flush()?;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn write_line(out: &mut impl Write, line: &Line, json: bool, colour: bool) -> Result<()> {
    if json {
        writeln!(out, "{}", line.raw)?;
        return Ok(());
    }
    match &line.entry {
        Entry::Record(record) => writeln!(out, "{}", render(record, colour))?,
        Entry::Raw(raw) => writeln!(out, "{raw}")?,
    }
    Ok(())
}

fn render(record: &Record, colour: bool) -> String {
    let mut out = String::new();
    let level = format!("{:LEVEL_WIDTH$}", record.level.as_str());
    if colour {
        out.push_str(&format!("{DIM}{}{RESET}", record.time_of_day()));
        out.push_str(COLUMN_GAP);
        out.push_str(&format!("{}{level}{RESET}", ansi(record.level)));
        out.push_str(COLUMN_GAP);
        out.push_str(&format!("{DIM}{}{RESET}", record.target));
    } else {
        out.push_str(record.time_of_day());
        out.push_str(COLUMN_GAP);
        out.push_str(&level);
        out.push_str(COLUMN_GAP);
        out.push_str(&record.target);
    }
    out.push_str(COLUMN_GAP);
    out.push_str(&record.message);

    for (key, value) in record
        .spans
        .iter()
        .flat_map(|span| span.fields.iter())
        .chain(record.fields.iter())
    {
        out.push_str(FIELD_GAP);
        out.push_str(key);
        out.push_str(KV_SEPARATOR);
        out.push_str(value);
    }
    out
}

fn ansi(level: Level) -> &'static str {
    match level {
        Level::Trace | Level::Debug => "\u{1b}[2m",
        Level::Info => "\u{1b}[32m",
        Level::Warn => "\u{1b}[33m",
        Level::Error => "\u{1b}[31m",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"WARN","fields":{"message":"retrying","attempt":3},"target":"caudra::provider","spans":[{"name":"turn","session_id":"s-1"}]}"#;
    const NOT_JSON: &str = "thread 'main' panicked";
    const ESCAPE: char = '\u{1b}';

    fn line(raw: &str) -> Line {
        Line {
            raw: raw.to_owned(),
            entry: Entry::parse(raw),
        }
    }

    fn printed(raw: &str, json: bool, colour: bool) -> String {
        let mut out = Vec::new();
        write_line(&mut out, &line(raw), json, colour).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_record_prints_time_level_target_message_and_every_field() {
        let out = printed(EVENT, false, false);
        assert!(out.contains("14:22:07.418"), "{out}");
        assert!(out.contains("WARN"), "{out}");
        assert!(out.contains("caudra::provider"), "{out}");
        assert!(out.contains("retrying"), "{out}");
        assert!(out.contains("attempt=3"), "{out}");
        assert!(out.contains("session_id=s-1"), "{out}");
    }

    #[test]
    fn a_pipe_gets_no_colour() {
        assert!(!printed(EVENT, false, false).contains(ESCAPE));
        assert!(printed(EVENT, false, true).contains(ESCAPE));
    }

    #[test]
    fn json_mode_hands_back_the_stored_bytes() {
        assert_eq!(printed(EVENT, true, true), format!("{EVENT}\n"));
    }

    #[test]
    fn an_unparseable_line_prints_verbatim() {
        assert_eq!(printed(NOT_JSON, false, false), format!("{NOT_JSON}\n"));
    }
}
