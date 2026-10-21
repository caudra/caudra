//! Port of upstream `store.ts`: the orchestrate store's operations over its plain files. Reads
//! take no lock; every write first takes the store's pid lock, which [`Store::close`] releases.

use std::fs;
use std::path::PathBuf;

use serde_json::json;

use crate::host::Host;

use super::compat::{MAX_SAFE_INTEGER, iso_timestamp, normalize, random_uuid};
use super::files::{
    EMPTY_FRONTIER, FRONTIER_FILE, GATES_FILE, INBOX_DIR, LEDGER_FILE, LEDGER_HEADER,
    PREFERENCES_FILE, STATUS_FILE, UNIT_HEADER, UNITS_FILE, atomic_write, exists,
    frontier_contents, not_initialized, pointer_contents, read_frontier, read_gates, read_ledger,
    read_pointers, read_standing, read_text, read_units, render_gates, required_cell,
    required_line, save_ledger, save_units, standing_contents, write_if_missing,
};
use super::forge::{Forge, resolve_frontier, validate_pin};
use super::lock::{LockTakeover, acquire, release};
use super::model::{
    Counts, Frontier, Gate, InboxPointer, InboxPushResult, LedgerEntry, NotFoundOutput, OrchError,
    OrchResult, PrState, StandingLine, StatusReport, Unit, parse_verdict,
};
use super::status::{changed, previous_summary, status_markdown, summarize};

const PENDING: &str = "pending";
const NOT_VERIFIED: &str = "NOT-VERIFIED";
const DRAIN_PREFIX: &str = ".inbox-drain-";

pub struct UnitUpdate<'a> {
    pub id: &'a str,
    pub state: &'a str,
    pub branch: Option<&'a str>,
    pub pr: Option<u64>,
    pub sha: Option<&'a str>,
}

pub struct LedgerRecord<'a> {
    pub pr: u64,
    pub sha: &'a str,
    pub verdict: &'a str,
    pub evidence: &'a str,
    pub verifier: Option<&'a str>,
}

pub struct GatePark<'a> {
    pub id: &'a str,
    pub question: &'a str,
    pub options: &'a str,
    pub default_answer: &'a str,
}

pub struct Store<'a> {
    host: &'a dyn Host,
    root: PathBuf,
    force: bool,
    on_takeover: Box<dyn FnMut(LockTakeover) + 'a>,
    locked: bool,
    closed: bool,
}

fn positive(value: u64, label: &str) -> OrchResult<String> {
    if value < 1 || value > MAX_SAFE_INTEGER.unsigned_abs() {
        return Err(OrchError::user(format!(
            "{label} must be a positive integer"
        )));
    }
    Ok(value.to_string())
}

fn optional_cell(value: Option<&str>, label: &str) -> OrchResult<String> {
    value.map_or(Ok(String::new()), |value| required_cell(value, label))
}

impl<'a> Store<'a> {
    /// Opens the store at `directory` without touching it; `on_takeover` hears of every lock
    /// this handle replaces.
    pub fn open(
        host: &'a dyn Host,
        directory: &str,
        force: bool,
        on_takeover: impl FnMut(LockTakeover) + 'a,
    ) -> Self {
        Self {
            host,
            root: normalize(&host.resolve(directory)),
            force,
            on_takeover: Box::new(on_takeover),
            locked: false,
            closed: false,
        }
    }

    fn ensure_open(&self) -> OrchResult<()> {
        if self.closed {
            return Err(OrchError::user("store is closed"));
        }
        Ok(())
    }

    fn ensure_lock(&mut self) -> OrchResult<()> {
        self.ensure_open()?;
        if !self.locked {
            acquire(self.host, &self.root, self.force, &mut self.on_takeover)?;
            self.locked = true;
        }
        Ok(())
    }

    fn begin_write(&mut self) -> OrchResult<()> {
        self.ensure_open()?;
        if !exists(&self.root)? {
            return Err(not_initialized(&self.root));
        }
        self.ensure_lock()
    }

    fn now(&self) -> String {
        iso_timestamp(self.host.now())
    }

    pub fn init(&mut self) -> OrchResult<PathBuf> {
        self.ensure_open()?;
        fs::create_dir_all(&self.root)?;
        self.ensure_lock()?;
        write_if_missing(&self.root.join(UNITS_FILE), &format!("{UNIT_HEADER}\n"))?;
        write_if_missing(&self.root.join(LEDGER_FILE), &format!("{LEDGER_HEADER}\n"))?;
        fs::create_dir_all(self.root.join(INBOX_DIR))?;
        write_if_missing(&self.root.join(GATES_FILE), "")?;
        write_if_missing(&self.root.join(PREFERENCES_FILE), "")?;
        write_if_missing(&self.root.join(FRONTIER_FILE), EMPTY_FRONTIER)?;
        Ok(self.root.clone())
    }

    pub fn add_unit(&mut self, id: &str, track: &str, brief: Option<&str>) -> OrchResult<Unit> {
        self.begin_write()?;
        let row = Unit {
            id: required_cell(id, "unit id")?,
            track: required_cell(track, "track")?,
            state: PENDING.to_owned(),
            branch: String::new(),
            pr: String::new(),
            sha: String::new(),
            brief: optional_cell(brief, "brief")?,
        };
        let mut rows = read_units(&self.root)?;
        if rows.iter().any(|unit| unit.id == row.id) {
            return Err(OrchError::user(format!("unit {} already exists", row.id)));
        }
        rows.push(row.clone());
        save_units(&self.root, &rows)?;
        Ok(row)
    }

    pub fn set_unit(&mut self, update: &UnitUpdate<'_>) -> OrchResult<Unit> {
        self.begin_write()?;
        let id = required_cell(update.id, "unit id")?;
        let state = required_cell(update.state, "state")?;
        let mut rows = read_units(&self.root)?;
        let Some(old) = rows.iter_mut().find(|unit| unit.id == id) else {
            return Err(OrchError::not_found(format!("unit {id} not found")));
        };
        let row = Unit {
            state,
            branch: match update.branch {
                Some(branch) => required_cell(branch, "branch")?,
                None => old.branch.clone(),
            },
            pr: match update.pr {
                Some(pr) => positive(pr, "PR")?,
                None => old.pr.clone(),
            },
            sha: match update.sha {
                Some(sha) => required_cell(sha, "SHA")?,
                None => old.sha.clone(),
            },
            ..old.clone()
        };
        *old = row.clone();
        save_units(&self.root, &rows)?;
        Ok(row)
    }

    pub fn unit(&self, id: &str) -> OrchResult<Unit> {
        self.ensure_open()?;
        let id = required_cell(id, "unit id")?;
        read_units(&self.root)?
            .into_iter()
            .find(|unit| unit.id == id)
            .ok_or_else(|| OrchError::not_found(format!("unit {id} not found")))
    }

    pub fn units(&self, state: Option<&str>, track: Option<&str>) -> OrchResult<Vec<Unit>> {
        self.ensure_open()?;
        let state = state
            .map(|state| required_cell(state, "state"))
            .transpose()?;
        let track = track
            .map(|track| required_cell(track, "track"))
            .transpose()?;
        let mut rows = read_units(&self.root)?;
        rows.retain(|unit| {
            state.as_ref().is_none_or(|state| unit.state == *state)
                && track.as_ref().is_none_or(|track| unit.track == *track)
        });
        Ok(rows)
    }

    pub fn unit_counts(&self) -> OrchResult<Counts> {
        self.ensure_open()?;
        let rows = read_units(&self.root)?;
        Ok(Counts::of(rows.iter().map(|unit| unit.state.as_str())))
    }

    pub fn record(&mut self, record: &LedgerRecord<'_>) -> OrchResult<LedgerEntry> {
        self.begin_write()?;
        let row = LedgerEntry {
            verdict: parse_verdict(record.verdict)?,
            pr: positive(record.pr, "PR")?,
            sha: required_cell(record.sha, "SHA")?,
            evidence: required_cell(record.evidence, "evidence")?,
            verifier: optional_cell(record.verifier, "verifier")?,
            ts: self.now(),
        };
        let mut rows = read_ledger(&self.root)?;
        match rows
            .iter_mut()
            .find(|old| old.pr == row.pr && old.sha == row.sha)
        {
            Some(old) => *old = row.clone(),
            None => rows.push(row.clone()),
        }
        save_ledger(&self.root, &rows)?;
        Ok(row)
    }

    pub fn check(&self, pr: u64, sha: &str) -> OrchResult<LedgerEntry> {
        self.ensure_open()?;
        let pr = positive(pr, "PR")?;
        let sha = required_cell(sha, "SHA")?;
        read_ledger(&self.root)?
            .into_iter()
            .find(|row| row.pr == pr && row.sha == sha)
            .ok_or_else(|| OrchError::NotFound {
                message: NOT_VERIFIED.to_owned(),
                output: Some(NotFoundOutput {
                    compact: NOT_VERIFIED.to_owned(),
                    json: json!({ "pr": pr, "sha": sha, "verdict": NOT_VERIFIED }),
                }),
            })
    }

    pub fn ledger_summary(&self) -> OrchResult<Counts> {
        self.ensure_open()?;
        let rows = read_ledger(&self.root)?;
        Ok(Counts::of(rows.iter().map(|row| row.verdict)))
    }

    pub fn push(
        &mut self,
        agent: &str,
        unit: &str,
        status: &str,
        report: Option<&str>,
    ) -> OrchResult<InboxPushResult> {
        self.begin_write()?;
        let pointer = InboxPointer {
            ts: self.now(),
            agent: required_cell(agent, "agent")?,
            unit: required_cell(unit, "unit")?,
            status: required_cell(status, "status")?,
            report: optional_cell(report, "report")?,
        };
        let inbox = self.root.join(INBOX_DIR);
        if !exists(&inbox)? {
            return Err(not_initialized(&self.root));
        }
        let filename = format!(
            "{}-{}-{}.tsv",
            pointer.ts.replace([':', '.'], "-"),
            std::process::id(),
            random_uuid()
        );
        atomic_write(&inbox.join(&filename), &pointer_contents(&pointer))?;
        Ok(InboxPushResult { pointer, filename })
    }

    /// Reads every pointer, then swaps the inbox for an empty one so arrivals during a drain
    /// wait for the next.
    pub fn drain(&mut self) -> OrchResult<Vec<InboxPointer>> {
        self.begin_write()?;
        let inbox = self.root.join(INBOX_DIR);
        let rows = read_pointers(&inbox)?;
        let drained = self.root.join(format!(
            "{DRAIN_PREFIX}{}-{}",
            std::process::id(),
            random_uuid()
        ));
        fs::rename(&inbox, &drained)?;
        if let Err(error) = fs::create_dir(&inbox) {
            fs::rename(&drained, &inbox)?;
            return Err(error.into());
        }
        match fs::remove_dir_all(&drained) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error.into());
            }
            _ => {}
        }
        Ok(rows)
    }

    pub fn peek(&self) -> OrchResult<Vec<InboxPointer>> {
        self.ensure_open()?;
        read_pointers(&self.root.join(INBOX_DIR))
    }

    pub fn inbox_count(&self) -> OrchResult<usize> {
        Ok(self.peek()?.len())
    }

    pub fn park(&mut self, params: &GatePark<'_>) -> OrchResult<Gate> {
        self.begin_write()?;
        let gate = Gate {
            id: required_line(params.id, "gate id")?,
            question: required_line(params.question, "question")?,
            options: required_line(params.options, "options")?,
            default_answer: required_line(params.default_answer, "default")?,
            answer: None,
        };
        let mut rows = read_gates(&self.root)?;
        match rows.iter_mut().find(|old| old.id == gate.id) {
            Some(old) => *old = gate.clone(),
            None => rows.push(gate.clone()),
        }
        atomic_write(&self.root.join(GATES_FILE), &render_gates(&rows))?;
        Ok(gate)
    }

    pub fn open_gates(&self) -> OrchResult<Vec<Gate>> {
        self.ensure_open()?;
        let mut rows = read_gates(&self.root)?;
        rows.retain(|gate| gate.answer.is_none());
        Ok(rows)
    }

    pub fn resolve_gate(&mut self, id: &str, answer: &str) -> OrchResult<Gate> {
        self.begin_write()?;
        let id = required_line(id, "gate id")?;
        let mut rows = read_gates(&self.root)?;
        let Some(old) = rows.iter_mut().find(|gate| gate.id == id) else {
            return Err(OrchError::not_found(format!("gate {id} not found")));
        };
        old.answer = Some(required_line(answer, "answer")?);
        let gate = old.clone();
        atomic_write(&self.root.join(GATES_FILE), &render_gates(&rows))?;
        Ok(gate)
    }

    pub fn set_frontier(
        &mut self,
        repo: &str,
        forge: &str,
        pin: Option<&[u64]>,
    ) -> OrchResult<Frontier> {
        self.begin_write()?;
        let repo = normalize(&self.host.resolve(&required_line(repo, "repo directory")?));
        let forge = Forge::parse(forge)?;
        if let Some(pin) = pin {
            for pr in pin {
                positive(*pr, "PR")?;
            }
            let mut distinct = pin.to_vec();
            distinct.sort_unstable();
            distinct.dedup();
            if distinct.len() != pin.len() {
                return Err(OrchError::user("--prs must not contain duplicates"));
            }
        }
        let old = read_frontier(&self.root)?;
        let prs = resolve_frontier(self.host, &repo, forge)?;
        if let Some(pin) = pin {
            let actual: Vec<u64> = prs.iter().map(|row| row.pr).collect();
            validate_pin(&actual, pin)?;
        }
        let lowest_unmerged = prs
            .iter()
            .find(|row| row.state == PrState::Open)
            .map(|row| row.pr.cast_signed());
        let value = Frontier {
            generation: old.generation + 1,
            prs,
            lowest_unmerged,
        };
        atomic_write(&self.root.join(FRONTIER_FILE), &frontier_contents(&value)?)?;
        Ok(value)
    }

    pub fn frontier(&self) -> OrchResult<Frontier> {
        self.ensure_open()?;
        read_frontier(&self.root)
    }

    pub fn standing(&self) -> OrchResult<Vec<StandingLine>> {
        self.ensure_open()?;
        read_standing(&self.root)
    }

    pub fn add_standing(&mut self, line: &str) -> OrchResult<StandingLine> {
        self.begin_write()?;
        let mut rows = read_standing(&self.root)?;
        let item = StandingLine {
            number: rows.len() + 1,
            line: required_line(line, "standing order")?,
        };
        rows.push(item.clone());
        atomic_write(&self.root.join(PREFERENCES_FILE), &standing_contents(&rows))?;
        Ok(item)
    }

    pub fn render_status(&mut self) -> OrchResult<StatusReport> {
        self.begin_write()?;
        let units = read_units(&self.root)?;
        let ledger = read_ledger(&self.root)?;
        let frontier = read_frontier(&self.root)?;
        let gates = read_gates(&self.root)?;
        let summary = summarize(&units, &ledger, &frontier, &gates);
        let path = self.root.join(STATUS_FILE);
        let before = if exists(&path)? {
            previous_summary(&read_text(&path)?)
        } else {
            None
        };
        let report = StatusReport {
            changed: changed(before.as_ref(), &summary),
            units,
            ledger,
            frontier,
            gates,
            summary,
        };
        let summary_json = serde_json::to_string(&report.summary).map_err(std::io::Error::other)?;
        atomic_write(&path, &status_markdown(&report, &self.now(), &summary_json))?;
        Ok(report)
    }

    pub fn close(&mut self) -> OrchResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if std::mem::take(&mut self.locked) {
            release(&self.root)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;

    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{GatePark, LedgerRecord, Store, UnitUpdate};
    use crate::host::Output;
    use crate::host::fake::FakeHost;
    use crate::scripts::orch::files::{INBOX_DIR, STATUS_FILE, UNITS_FILE};
    use crate::scripts::orch::lock::{LOCK_FILE, LockTakeover, NO_SUCH_PROCESS};
    use crate::scripts::orch::model::{
        FrontierPr, OrchError, PrState, StandingLine, Unit, parse_verdict,
    };

    const MERGED_SHA: &str = "1111111111111111111111111111111111111111";
    const CLOSED_SHA: &str = "2222222222222222222222222222222222222222";
    const OPEN_SHA: &str = "3333333333333333333333333333333333333333";
    const DRAFT_SHA: &str = "4444444444444444444444444444444444444444";
    const DEAD_PID: &str = "4194305";
    const STORE_CLOSED: &str = "store is closed";
    const CHAIN_PIN_MISSING: &str =
        "frontier pin mismatch: missing from resolved chain: 12; extra in resolved chain: 13,14";
    const CHAIN_PIN_ORDER: &str =
        "frontier pin mismatch: order differs: expected 13,10,11,14; resolved 10,13,11,14";
    const DUPLICATE_PIN: &str = "--prs must not contain duplicates";
    const NO_PULL_REQUEST: &str = "branch stack/open has no pull request; open one with gh pr create, or resolve the frontier from a branch whose chain is fully submitted";

    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("readable")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn message(error: OrchError) -> String {
        error.to_string()
    }

    fn open<'a>(host: &'a FakeHost, directory: &Path) -> Store<'a> {
        Store::open(host, &directory.to_string_lossy(), false, |_| {})
    }

    fn initialized<'a>(host: &'a FakeHost, directory: &Path) -> Store<'a> {
        let mut store = open(host, directory);
        store.init().expect("initialized");
        store
    }

    fn update<'a>(id: &'a str, state: &'a str) -> UnitUpdate<'a> {
        UnitUpdate {
            id,
            state,
            branch: None,
            pr: None,
            sha: None,
        }
    }

    fn record<'a>(
        verdict: &'a str,
        evidence: &'a str,
        verifier: Option<&'a str>,
    ) -> LedgerRecord<'a> {
        LedgerRecord {
            pr: 184_530,
            sha: "abc123",
            verdict,
            evidence,
            verifier,
        }
    }

    fn forge_row(forge: &str, number: u64, state: &str, head: &str, base: &str) -> Value {
        if forge == "gh" {
            let state = if state == "draft" {
                "OPEN".to_owned()
            } else {
                state.to_uppercase()
            };
            json!({ "number": number, "state": state, "headRefName": head, "baseRefName": base })
        } else {
            json!({ "number": number, "status": state, "headRef": head, "baseRef": base })
        }
    }

    fn list(forge: &str, filter: &str, branch: &str) -> String {
        let fields = if forge == "gh" {
            "number,state,headRefName,baseRefName"
        } else {
            "number,status,headRef,baseRef"
        };
        format!("{forge} pr list {filter} {branch} --state all --json {fields}")
    }

    /// The stack upstream's fixture builds: main <- merged #10 <- closed #13 <- open #11 (HEAD)
    /// <- draft #14, plus a closed #15 that also targets the open branch.
    fn script_stack(host: &FakeHost, forge: &str, current_rows: Option<&str>) {
        let rows = |rows: Vec<Value>| Output::success(format!("{}\n", Value::Array(rows)));
        host.script(
            "git symbolic-ref --short refs/remotes/origin/HEAD",
            Output::failure(
                128,
                "fatal: ref refs/remotes/origin/HEAD is not a symbolic ref\n",
            ),
        );
        host.script(
            "git symbolic-ref --short HEAD",
            Output::success("stack/open\n"),
        );
        for (branch, sha) in [
            ("stack/merged", MERGED_SHA),
            ("stack/closed", CLOSED_SHA),
            ("stack/open", OPEN_SHA),
            ("stack/draft", DRAFT_SHA),
        ] {
            host.script(
                &format!("git rev-parse {branch}"),
                Output::success(format!("{sha}\n")),
            );
        }
        let current = current_rows.map_or_else(
            || {
                rows(vec![forge_row(
                    forge,
                    11,
                    "open",
                    "stack/open",
                    "stack/closed",
                )])
            },
            |raw| Output::success(format!("{raw}\n")),
        );
        host.script(&list(forge, "--head", "stack/open"), current);
        host.script(
            &list(forge, "--head", "stack/closed"),
            rows(vec![forge_row(
                forge,
                13,
                "closed",
                "stack/closed",
                "stack/merged",
            )]),
        );
        host.script(
            &list(forge, "--head", "stack/merged"),
            rows(vec![forge_row(forge, 10, "merged", "stack/merged", "main")]),
        );
        host.script(
            &list(forge, "--base", "stack/open"),
            rows(vec![
                forge_row(forge, 14, "draft", "stack/draft", "stack/open"),
                forge_row(forge, 15, "closed", "stack/abandoned", "stack/open"),
            ]),
        );
        host.script(&list(forge, "--base", "stack/draft"), rows(Vec::new()));
    }

    #[test]
    fn initializes_an_idempotent_plain_file_store_and_releases_its_lock() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = open(&host, directory.path());

        assert_eq!(store.init().expect("init"), directory.path());
        let units = fs::read(directory.path().join(UNITS_FILE)).expect("units");
        assert_eq!(store.init().expect("init again"), directory.path());
        assert_eq!(
            fs::read(directory.path().join(UNITS_FILE)).expect("units"),
            units
        );
        assert_eq!(
            names(directory.path()),
            [
                LOCK_FILE,
                "frontier.json",
                "gates.md",
                INBOX_DIR,
                "ledger.tsv",
                "preferences.md",
                UNITS_FILE
            ]
        );
        assert_eq!(
            fs::read_to_string(directory.path().join(LOCK_FILE)).expect("lock"),
            format!("{}\n", std::process::id())
        );

        store.close().expect("close");
        assert!(!names(directory.path()).contains(&LOCK_FILE.to_owned()));
    }

    #[test]
    fn composes_unit_add_set_get_list_and_counts() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());

        let added = store
            .add_unit("u1", "build", Some("briefs/u1.md"))
            .expect("add");
        assert_eq!((added.id.as_str(), added.state.as_str()), ("u1", "pending"));
        let formula = store
            .add_unit("=SUM(A1)", "+build", None)
            .expect("add formula");
        assert_eq!(
            (formula.id.as_str(), formula.track.as_str()),
            ("'=SUM(A1)", "'+build")
        );

        let updated = store
            .set_unit(&UnitUpdate {
                branch: Some("poteto/u1"),
                pr: Some(184_530),
                sha: Some("abc123"),
                ..update("u1", "done")
            })
            .expect("set");
        assert_eq!(
            updated,
            Unit {
                id: "u1".to_owned(),
                track: "build".to_owned(),
                state: "done".to_owned(),
                branch: "poteto/u1".to_owned(),
                pr: "184530".to_owned(),
                sha: "abc123".to_owned(),
                brief: "briefs/u1.md".to_owned(),
            }
        );
        assert_eq!(store.unit("u1").expect("get"), updated);
        assert_eq!(
            store.units(Some("done"), Some("build")).expect("list"),
            [updated]
        );
        assert_eq!(
            store.unit_counts().expect("counts").line(),
            "done=1, pending=1"
        );
        assert_eq!(
            message(store.add_unit("u1", "build", None).expect_err("duplicate")),
            "unit u1 already exists"
        );
        assert!(matches!(
            store.set_unit(&update("missing", "done")),
            Err(OrchError::NotFound { output: None, .. })
        ));
    }

    #[test]
    fn records_replaces_checks_and_summarizes_typed_ledger_verdicts() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());

        let Err(OrchError::NotFound {
            output: Some(output),
            ..
        }) = store.check(184_530, "abc123")
        else {
            panic!("expected NOT-VERIFIED");
        };
        assert_eq!(output.compact, "NOT-VERIFIED");
        assert_eq!(
            output.json,
            json!({ "pr": "184530", "sha": "abc123", "verdict": "NOT-VERIFIED" })
        );
        assert!(
            message(parse_verdict("looks-good").expect_err("invalid"))
                .starts_with("verdict must be")
        );

        let recorded = store
            .record(&record(
                "unit-test-verified",
                "reports/verify.md",
                Some("opus"),
            ))
            .expect("record");
        assert_eq!(recorded.ts, "1970-01-01T00:00:00.000Z");
        assert_eq!(store.check(184_530, "abc123").expect("check"), recorded);
        assert_eq!(
            store.ledger_summary().expect("summary").line(),
            "unit-test-verified=1"
        );

        store
            .record(&record("live-ui-verified", "reports/live.md", None))
            .expect("replace");
        assert_eq!(
            store.ledger_summary().expect("summary").line(),
            "live-ui-verified=1"
        );
    }

    #[test]
    fn pushes_peeks_and_atomically_drains_inbox_pointers() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());

        let first = store
            .push("worker-1", "u1", "done", Some("reports/u1.md"))
            .expect("push");
        assert_eq!(
            (first.pointer.unit.as_str(), first.pointer.status.as_str()),
            ("u1", "done")
        );
        assert!(first.filename.starts_with("1970-01-01T00-00-00-000Z-"));
        assert!(first.filename.ends_with(".tsv"));
        assert_eq!(
            fs::read_to_string(directory.path().join(INBOX_DIR).join(&first.filename))
                .expect("pointer"),
            "1970-01-01T00:00:00.000Z\tworker-1\tu1\tdone\treports/u1.md\n"
        );
        store.push("worker-2", "u2", "failed", None).expect("push");

        assert_eq!(store.inbox_count().expect("count"), 2);
        assert_eq!(store.peek().expect("peek").len(), 2);
        assert_eq!(store.inbox_count().expect("count"), 2);
        assert_eq!(store.drain().expect("drain").len(), 2);
        assert_eq!(store.inbox_count().expect("count"), 0);
        assert!(names(&directory.path().join(INBOX_DIR)).is_empty());
        assert!(
            !names(directory.path())
                .iter()
                .any(|name| name.starts_with(".inbox-drain-"))
        );
    }

    #[test]
    fn replaces_a_stale_lock_whose_holder_pid_is_dead() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        initialized(&host, directory.path()).close().expect("close");
        fs::write(directory.path().join(LOCK_FILE), format!("{DEAD_PID}\n")).expect("lock");
        host.script(
            &format!("kill -0 {DEAD_PID}"),
            Output::failure(1, format!("kill: ({DEAD_PID}): {NO_SUCH_PROCESS}\n")),
        );

        let takeovers = RefCell::new(Vec::new());
        let mut recovered = Store::open(
            &host,
            &directory.path().to_string_lossy(),
            false,
            |takeover| {
                takeovers.borrow_mut().push(takeover);
            },
        );
        assert_eq!(
            recovered.add_unit("u1", "build", None).expect("add").id,
            "u1"
        );
        recovered.close().expect("close");
        assert_eq!(
            *takeovers.borrow(),
            [LockTakeover::Stale(DEAD_PID.to_owned())]
        );
        assert!(!names(directory.path()).contains(&LOCK_FILE.to_owned()));
    }

    #[test]
    fn blocks_a_writer_and_steals_the_pid_lock_only_with_force() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        initialized(&host, directory.path()).close().expect("close");
        let pid = std::process::id().to_string();
        fs::write(directory.path().join(LOCK_FILE), format!("{pid}\n")).expect("lock");
        host.script(&format!("kill -0 {pid}"), Output::success(""));

        let mut blocked = open(&host, directory.path());
        assert_eq!(
            message(blocked.add_unit("u1", "build", None).expect_err("blocked")),
            format!("store lock held by pid {pid}")
        );

        let takeovers = RefCell::new(Vec::new());
        let mut forced = Store::open(
            &host,
            &directory.path().to_string_lossy(),
            true,
            |takeover| {
                takeovers.borrow_mut().push(takeover);
            },
        );
        assert_eq!(forced.add_unit("u1", "build", None).expect("add").id, "u1");
        forced.close().expect("close");
        assert_eq!(*takeovers.borrow(), [LockTakeover::Stolen(pid)]);
        assert!(!names(directory.path()).contains(&LOCK_FILE.to_owned()));
    }

    #[test]
    fn parks_gates_stores_standing_orders_and_renders_status() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());
        store.add_unit("u1", "build", None).expect("add");
        let gate = store
            .park(&GatePark {
                id: "release",
                question: "Ship now?",
                options: "ship,wait",
                default_answer: "wait",
            })
            .expect("park");
        assert_eq!((gate.kind(), gate.id.as_str()), ("open", "release"));
        assert_eq!(
            store.add_standing("Never force push.").expect("standing"),
            StandingLine {
                number: 1,
                line: "Never force push.".to_owned()
            }
        );

        let first = store.render_status().expect("render");
        assert_eq!(first.changed, "first render");
        assert_eq!(first.summary.open_gate_ids, ["release"]);
        let page = fs::read_to_string(directory.path().join(STATUS_FILE)).expect("status");
        assert!(page.contains("| release | open | Ship now? |"));
        assert_eq!(
            store.render_status().expect("render").changed,
            "no derived changes"
        );

        let resolved = store.resolve_gate("release", "ship").expect("resolve");
        assert_eq!(
            (resolved.kind(), resolved.answer.as_deref()),
            ("resolved", Some("ship"))
        );
        assert_eq!(
            store.render_status().expect("render").changed,
            "open gates 1->0"
        );
        assert!(store.open_gates().expect("gates").is_empty());
        assert_eq!(store.standing().expect("standing").len(), 1);
    }

    #[test_case("gh" ; "gh")]
    #[test_case("origin" ; "origin")]
    fn resolves_the_ordered_frontier_from_the_base_chain_and_validates_a_pin(forge: &str) {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());
        script_stack(&host, forge, None);

        let frontier = store.set_frontier("repo", forge, None).expect("frontier");
        let row = |pr, branches: &str, sha: &str, state| FrontierPr {
            pr,
            branches: branches.to_owned(),
            sha: sha.to_owned(),
            state,
        };
        assert_eq!(frontier.generation, 1);
        assert_eq!(
            frontier.prs,
            [
                row(10, "stack/merged", MERGED_SHA, PrState::Merged),
                row(13, "stack/closed", CLOSED_SHA, PrState::Closed),
                row(11, "stack/open", OPEN_SHA, PrState::Open),
                row(14, "stack/draft", DRAFT_SHA, PrState::Open),
            ]
        );
        assert_eq!(frontier.lowest_unmerged, Some(11));
        assert_eq!(
            store
                .set_frontier("repo", forge, Some(&[10, 13, 11, 14]))
                .expect("pinned")
                .generation,
            2
        );
        assert_eq!(store.frontier().expect("show").generation, 2);
        for (pin, expected) in [
            (&[10, 11, 12][..], CHAIN_PIN_MISSING),
            (&[13, 10, 11, 14][..], CHAIN_PIN_ORDER),
            (&[10, 10][..], DUPLICATE_PIN),
        ] {
            assert_eq!(
                message(
                    store
                        .set_frontier("repo", forge, Some(pin))
                        .expect_err("pin")
                ),
                expected
            );
        }
    }

    #[test_case("gh" ; "gh")]
    #[test_case("origin" ; "origin")]
    fn rejects_unparseable_forge_output_loudly(forge: &str) {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());
        script_stack(&host, forge, Some("this line is not forge output"));

        assert_eq!(
            message(
                store
                    .set_frontier("repo", forge, None)
                    .expect_err("unparseable")
            ),
            format!("{forge} pr list --head stack/open output is not valid JSON")
        );
    }

    #[test]
    fn names_the_missing_pull_request_when_a_chain_branch_has_none() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());
        script_stack(&host, "gh", Some("[]"));

        assert_eq!(
            message(store.set_frontier("repo", "gh", None).expect_err("missing")),
            NO_PULL_REQUEST
        );
    }

    #[test]
    fn rejects_malformed_tsv_verdict_frontier_and_inbox_data() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let store = initialized(&host, directory.path());
        let write = |name: &str, contents: &str| {
            fs::write(directory.path().join(name), contents).expect("write");
        };

        write(UNITS_FILE, "wrong\n");
        assert_eq!(
            message(store.units(None, None).expect_err("header")),
            "units.tsv has an invalid header"
        );
        write(
            UNITS_FILE,
            "id\ttrack\tstate\tbranch\tpr\tsha\tbrief\nshort\trow\n",
        );
        assert_eq!(
            message(store.units(None, None).expect_err("row")),
            "units.tsv has a malformed row"
        );
        write(
            "ledger.tsv",
            "pr\tsha\tverdict\tevidence\tverifier\tts\n1\tsha\tinvalid\treport\tme\tnow\n",
        );
        assert_eq!(
            message(store.ledger_summary().expect_err("verdict")),
            "ledger.tsv has invalid verdict invalid"
        );
        write("frontier.json", "{\"generation\":\"1\"}\n");
        assert_eq!(
            message(store.frontier().expect_err("shape")),
            "frontier.json has an invalid shape"
        );
        write("inbox/bad.tsv", "too\tshort\n");
        assert_eq!(
            message(store.peek().expect_err("pointer")),
            "inbox pointer bad.tsv is malformed"
        );
    }

    #[test]
    fn rejects_operations_after_close() {
        let directory = TempDir::new().expect("tempdir");
        let host = FakeHost::new(directory.path());
        let mut store = initialized(&host, directory.path());
        store.close().expect("close");

        assert_eq!(
            message(store.units(None, None).expect_err("closed")),
            STORE_CLOSED
        );
        assert!(matches!(store.render_status(), Err(OrchError::User(_))));
    }
}
