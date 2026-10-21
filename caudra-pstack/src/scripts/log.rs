//! Port of show-me-your-work's `log.sh`: appends one well-formed row to a decision log (TSV).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use crate::host::{Host, Output};

const USAGE: &str = "usage: log.sh <logfile> <phase> <decision> <why> <evidence> <result>\n";
const HEADER: &str = "ts\tphase\tdecision\twhy\tevidence\tresult\n";
const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
const LINE_BREAKERS: [char; 3] = ['\t', '\n', '\r'];
const FORMULA_LEADS: [char; 4] = ['=', '+', '-', '@'];

pub fn run(host: &dyn Host, argv: &[String]) -> Output {
    let [logfile, phase, decision, why, evidence, result] = argv else {
        return Output::failure(1, USAGE);
    };
    let timestamp = host.now().strftime(TIMESTAMP_FORMAT).to_string();
    let row = [phase, decision, why, evidence, result]
        .into_iter()
        .fold(timestamp, |row, cell| format!("{row}\t{}", clean(cell)));
    match append(&host.resolve(logfile), &row) {
        Ok(()) => Output::default(),
        Err(error) => Output::failure(1, format!("log.sh: {logfile}: {error}\n")),
    }
}

/// Never truncates: a log that exists but reads as empty costs one stray header, not its rows.
fn append(path: &Path, row: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let header = if file.metadata()?.len() == 0 {
        HEADER
    } else {
        ""
    };
    file.write_all(format!("{header}{row}\n").as_bytes())
}

/// Keeps a cell on one line, and quotes one a spreadsheet would run as a formula, since the log
/// carries attacker-controlled evidence such as PR titles.
fn clean(cell: &str) -> String {
    let flat = cell.replace(LINE_BREAKERS, " ");
    if flat.starts_with(FORMULA_LEADS) {
        format!("'{flat}")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use std::iter;

    use jiff::Timestamp;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{HEADER, USAGE, fs, run};
    use crate::host::fake::FakeHost;

    const LOG: &str = "logs/run/decisions.tsv";
    const NOW: &str = "2026-10-20T08:09:10Z";

    fn args(cells: &[&str]) -> Vec<String> {
        iter::once(LOG)
            .chain(cells.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    fn host(dir: &TempDir) -> FakeHost {
        FakeHost::new(dir.path()).at(NOW.parse::<Timestamp>().expect("fixed timestamp"))
    }

    fn read_log(dir: &TempDir) -> String {
        fs::read_to_string(dir.path().join(LOG)).expect("log written")
    }

    #[test_case(&[]; "no arguments")]
    #[test_case(&["phase", "decision", "why", "evidence"]; "one cell short")]
    #[test_case(&["phase", "decision", "why", "evidence", "result", "extra"]; "one cell over")]
    fn wrong_arity_prints_usage(cells: &[&str]) {
        let dir = TempDir::new().expect("tempdir");
        let output = run(&host(&dir), &args(cells));
        assert_eq!((output.code, output.stderr.as_str()), (1, USAGE));
        assert!(!dir.path().join(LOG).exists());
    }

    #[test]
    fn first_row_creates_directory_and_header() {
        let dir = TempDir::new().expect("tempdir");
        let output = run(
            &host(&dir),
            &args(&["plan", "pick A", "faster", "bench.txt", "ok"]),
        );
        assert_eq!(output.code, 0);
        assert_eq!(
            read_log(&dir),
            format!("{HEADER}{NOW}\tplan\tpick A\tfaster\tbench.txt\tok\n")
        );
    }

    #[test]
    fn later_rows_append_without_header() {
        let dir = TempDir::new().expect("tempdir");
        let host = host(&dir);
        run(&host, &args(&["one", "a", "b", "c", "d"]));
        run(&host, &args(&["two", "a", "b", "c", "d"]));
        assert_eq!(
            read_log(&dir),
            format!("{HEADER}{NOW}\tone\ta\tb\tc\td\n{NOW}\ttwo\ta\tb\tc\td\n")
        );
    }

    #[test]
    fn existing_rows_are_kept_and_get_no_header() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join(LOG);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "kept\n").expect("seed");
        run(&host(&dir), &args(&["p", "d", "w", "e", "r"]));
        assert_eq!(read_log(&dir), format!("kept\n{NOW}\tp\td\tw\te\tr\n"));
    }

    #[test_case("a\tb\nc\rd", "a b c d"; "line breakers become spaces")]
    #[test_case("=SUM(A1)", "'=SUM(A1)"; "equals")]
    #[test_case("+1", "'+1"; "plus")]
    #[test_case("-cmd", "'-cmd"; "minus")]
    #[test_case("@here", "'@here"; "at")]
    #[test_case("\n=1", " =1"; "guard checks the cleaned cell")]
    #[test_case("a=b", "a=b"; "formula lead after the start")]
    fn cells_are_cleaned(cell: &str, expected: &str) {
        let dir = TempDir::new().expect("tempdir");
        run(&host(&dir), &args(&[cell, "d", "w", "e", "r"]));
        assert_eq!(
            read_log(&dir),
            format!("{HEADER}{NOW}\t{expected}\td\tw\te\tr\n")
        );
    }
}
