use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Args, Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use jiff::{SignedDuration, Timestamp, ToSpan, Zoned, civil::Date};
use schemars::schema_for;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    CatalogProvider, Provider, Snapshot, TRUSTED_KEYS, audit, hex, import_models_dev,
    parse_bounded, replay,
    repo::{self, DecisionFile},
    routing, sign, to_pretty_bytes, validate, validate_successor,
};

const SIGNING_KEY_ENV: &str = "CAUDRA_MODELS_SIGNING_KEY";
const ZSTD_LEVEL: i32 = 19;
const HASH_MESSAGE: &str = "content hash does not match payload";
const PROVIDER_SCHEMA: &str = "provider.v1.json";
const MODELS_SCHEMA: &str = "models.v1.json";
const DECISIONS_SCHEMA: &str = "decision-models.v1.json";
const DEFAULT_LOOKBACK_DAYS: i64 = 90;
const MAX_CLOCK_SKEW_MINUTES: i64 = 5;
const FUTURE_MESSAGE: &str = "minutes ahead of this machine's clock";
const STAMP_MESSAGE: &str =
    "the content changed, so updated_at must be later than the committed file's";
const PIN_MESSAGE: &str = "is not newer than the current pin";

type Files = Vec<(PathBuf, Vec<u8>)>;

#[derive(Parser)]
#[command(
    name = "caudra-models",
    about = "Build, validate, and sign model files without network access"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Replay fixed automation events and check their outcomes without performing actions.
    Replay { script: PathBuf, cases: PathBuf },
    /// Compare curated models with a local models.dev catalog. Never downloads data.
    Audit {
        data_dir: PathBuf,
        /// Defaults to the repository's reviewed import/models-dev.json.
        #[arg(long)]
        models_dev: Option<PathBuf>,
        /// The published models.json whose catalog routing the report compares with.
        #[arg(long)]
        published: Option<PathBuf>,
        #[arg(long)]
        since: Option<Date>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate a snapshot or check a repository's generated files.
    Validate {
        path: PathBuf,
        /// The published models.json a repository's data must stay compatible with.
        #[arg(long)]
        published: Option<PathBuf>,
    },
    /// Build a snapshot and JSON schemas from an authoring repository.
    Build {
        repo: PathBuf,
        #[arg(long)]
        now: Option<Timestamp>,
    },
    /// Filter a downloaded models.dev catalog into an authoring repository.
    ImportModelsDev { api: PathBuf, repo: PathBuf },
    /// Write the authoring and snapshot JSON schemas.
    Schema { dir: PathBuf },
    /// Create a private signing key without overwriting an existing file.
    Keygen { pkcs8_path: PathBuf },
    /// Sign exact snapshot bytes with CAUDRA_MODELS_SIGNING_KEY.
    Sign { models: PathBuf },
    /// Verify an exact-byte signature against trusted public keys.
    Verify {
        models: PathBuf,
        sig: Option<PathBuf>,
        #[command(flatten)]
        keys: Keys,
    },
    /// Validate and compress a snapshot for bundling in a release.
    Pin {
        models: PathBuf,
        output: PathBuf,
        #[arg(
            long,
            required_unless_present = "unsigned",
            conflicts_with = "unsigned"
        )]
        sig: Option<PathBuf>,
        #[arg(long, conflicts_with = "sig")]
        unsigned: bool,
        /// Replace a pin whose updated_at is not older than this file's.
        #[arg(long)]
        allow_older: bool,
        #[command(flatten)]
        keys: Keys,
    },
}

#[derive(Args)]
struct Keys {
    /// Trust this base64 Ed25519 public key instead of the production key list.
    #[arg(long = "key")]
    values: Vec<String>,
}

impl Keys {
    fn decode(&self) -> Result<Vec<[u8; 32]>> {
        if self.values.is_empty() {
            return Ok(TRUSTED_KEYS.to_vec());
        }
        self.values
            .iter()
            .map(|key| {
                STANDARD
                    .decode(key)
                    .wrap_err("--key must be base64")?
                    .try_into()
                    .map_err(|_| eyre!("--key must decode to exactly 32 bytes"))
            })
            .collect()
    }
}

pub fn run() -> Result<()> {
    execute(Cli::parse().command, Timestamp::now())
}

fn execute(command: Command, clock: Timestamp) -> Result<()> {
    match command {
        Command::Replay { script, cases } => {
            let source = String::from_utf8(read(&script)?).wrap_err("script must be UTF-8")?;
            let cases = repo::read_json(&cases)?;
            let count = replay::check(&source, cases)?;
            println!("{count} replay cases passed");
            Ok(())
        }
        Command::Audit {
            data_dir,
            models_dev,
            published,
            since,
            out,
        } => {
            let payload = repo::load(&data_dir)?;
            let rules = audit::load_rules(&data_dir, &payload.providers)?;
            let catalog = models_dev.map(|path| repo::read_json(&path)).transpose()?;
            let published = published.as_deref().map(read_published).transpose()?;
            let today = Zoned::now().date();
            let since =
                since.map_or_else(|| today.checked_sub(DEFAULT_LOOKBACK_DAYS.days()), Ok)?;
            let report = audit::audit(
                &payload,
                catalog.as_ref().unwrap_or(&payload.catalog),
                published.as_ref().map(|published| &published.payload),
                today,
                since,
                &rules,
            )
            .to_string();
            if let Some(path) = out {
                write(&path, report.as_bytes())
            } else {
                print!("{report}");
                Ok(())
            }
        }
        Command::Validate { path, published } => validate_path(&path, published.as_deref(), clock),
        Command::Build { repo, now } => build(&repo, now.unwrap_or(clock), clock),
        Command::ImportModelsDev { api, repo } => {
            let slugs = routing::builtin_slugs(&repo::load_providers(&repo)?);
            let path = repo.join(repo::IMPORT);
            let previous: BTreeMap<String, CatalogProvider> = if path.exists() {
                repo::read_json(&path)?
            } else {
                BTreeMap::new()
            };
            let imported = import_models_dev(&read(&api)?, &slugs)
                .wrap_err_with(|| format!("{}: invalid catalog JSON", api.display()))?;
            write_json(&path, &imported.catalog)?;
            for dropped in &imported.dropped_apis {
                println!("{dropped}");
            }
            let changes = routing::changes(&slugs, &previous, &imported.catalog);
            print!("{}", routing::section(Some(&changes)));
            Ok(())
        }
        Command::Schema { dir } => {
            for (name, bytes) in schemas()? {
                write(&dir.join(name), &bytes)?;
            }
            Ok(())
        }
        Command::Keygen { pkcs8_path } => keygen(&pkcs8_path),
        Command::Sign { models } => {
            let key =
                STANDARD
                    .decode(std::env::var(SIGNING_KEY_ENV).wrap_err_with(|| {
                        format!("set {SIGNING_KEY_ENV} to a base64 PKCS#8 key")
                    })?)
                    .wrap_err("signing key must be base64")?;
            let bytes = read(&models)?;
            checked_snapshot(&models, &bytes)?;
            write_json(
                &append_extension(&models, "sig"),
                &sign::sign(&bytes, &key)?,
            )
        }
        Command::Verify { models, sig, keys } => {
            let sig = sig.unwrap_or_else(|| append_extension(&models, "sig"));
            verify_signature(&models, &read(&models)?, &sig, &keys.decode()?)
        }
        Command::Pin {
            models,
            output,
            sig,
            unsigned: _,
            allow_older,
            keys,
        } => pin(
            &models,
            &output,
            sig.as_deref(),
            &keys.decode()?,
            allow_older,
        ),
    }
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).wrap_err_with(|| format!("reading {}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent).wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, bytes).wrap_err_with(|| format!("writing {}", path.display()))
}

fn pretty(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write(path, &pretty(value)?)
}

fn checked_snapshot(path: &Path, bytes: &[u8]) -> Result<Snapshot> {
    let snapshot =
        parse_bounded(bytes).wrap_err_with(|| format!("{}: invalid snapshot", path.display()))?;
    validate(&snapshot).map_err(|issues| {
        eyre!(
            "{}:\n{}",
            path.display(),
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    if !snapshot.hash_matches() {
        bail!("{}: /content_sha256: {HASH_MESSAGE}", path.display());
    }
    Ok(snapshot)
}

fn schemas() -> Result<Vec<(&'static str, Vec<u8>)>> {
    Ok(vec![
        (PROVIDER_SCHEMA, pretty(&schema_for!(Provider))?),
        (MODELS_SCHEMA, pretty(&schema_for!(Snapshot))?),
        (DECISIONS_SCHEMA, pretty(&schema_for!(DecisionFile))?),
    ])
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).wrap_err_with(|| format!("reading {}", path.display())),
    }
}

fn read_published(path: &Path) -> Result<Snapshot> {
    parse_bounded(&read(path)?).wrap_err_with(|| format!("{}: invalid snapshot", path.display()))
}

fn not_in_future(path: &Path, at: Timestamp, clock: Timestamp) -> Result<()> {
    if at > clock + SignedDuration::from_mins(MAX_CLOCK_SKEW_MINUTES) {
        bail!(
            "{}: updated_at {at} is more than {MAX_CLOCK_SKEW_MINUTES} {FUTURE_MESSAGE} ({clock})",
            path.display()
        );
    }
    Ok(())
}

/// The built snapshot and every generated file, stamped against `previous`.
fn artifacts(dir: &Path, previous: Option<&Snapshot>, now: Timestamp) -> Result<(Snapshot, Files)> {
    let path = dir.join(repo::DIST);
    let payload = repo::load(dir)?;
    audit::load_rules(dir, &payload.providers)?;
    let mut snapshot = Snapshot::stamp(payload, previous, now);
    snapshot.schema = Some("schema.json".to_owned());
    let bytes = to_pretty_bytes(&snapshot);
    checked_snapshot(&path, &bytes)?;
    let mut artifacts = vec![(path, bytes)];
    for (name, bytes) in schemas()? {
        if name == MODELS_SCHEMA {
            artifacts.push((dir.join("dist/v1/schema.json"), bytes.clone()));
        }
        artifacts.push((dir.join("schema").join(name), bytes));
    }
    Ok((snapshot, artifacts))
}

fn build(dir: &Path, now: Timestamp, clock: Timestamp) -> Result<()> {
    let path = dir.join(repo::DIST);
    not_in_future(&path, now, clock)?;
    let previous = read_optional(&path)?
        .map(|bytes| checked_snapshot(&path, &bytes))
        .transpose()?;
    let (snapshot, files) = artifacts(dir, previous.as_ref(), now)?;
    if let Some(previous) = &previous
        && !matches!(
            snapshot.is_newer_than(previous),
            Ok(Ordering::Greater | Ordering::Equal)
        )
    {
        bail!(
            "{}: {STAMP_MESSAGE} ({}); pass a later --now",
            path.display(),
            previous.updated_at
        );
    }
    for (path, bytes) in files {
        write(&path, &bytes)?;
    }
    Ok(())
}

fn validate_path(path: &Path, published: Option<&Path>, clock: Timestamp) -> Result<()> {
    if !path.is_dir() {
        let snapshot = checked_snapshot(path, &read(path)?)?;
        return not_in_future(path, snapshot.updated_at, clock);
    }
    let existing = path.join(repo::DIST);
    let previous = checked_snapshot(&existing, &read(&existing)?)?;
    not_in_future(&existing, previous.updated_at, clock)?;
    let (snapshot, files) = artifacts(path, Some(&previous), previous.updated_at)?;
    for (file, expected) in files {
        if read(&file)? != expected {
            bail!(
                "{}: generated file is stale; run build {}",
                file.display(),
                path.display()
            );
        }
    }
    let Some(published) = published else {
        return Ok(());
    };
    let published = read_published(published)?;
    let rules = audit::load_rules(path, &snapshot.payload.providers)?;
    let mut issues = validate_successor(&published.payload, &snapshot.payload)
        .err()
        .unwrap_or_default();
    let (routing, _) = rules.unaccepted_routing(audit::routing_changes(
        &published.payload,
        &snapshot.payload,
    ));
    issues.extend(routing.iter().map(routing::RoutingChange::issue));
    if issues.is_empty() {
        return Ok(());
    }
    bail!(
        "{}:\n{}",
        existing.display(),
        issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn append_extension(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{extension}"));
    name.into()
}

fn keygen(path: &Path) -> Result<()> {
    let (pkcs8, public) = sign::keygen()?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).wrap_err_with(|| {
        format!(
            "creating private key {} without overwriting",
            path.display()
        )
    })?;
    file.write_all(&pkcs8)?;
    println!(
        "public_key = {}\nkey_id = {}",
        STANDARD.encode(public),
        sign::key_id(&public)
    );
    Ok(())
}

fn verify_signature(models: &Path, bytes: &[u8], sig: &Path, trusted: &[[u8; 32]]) -> Result<()> {
    let signature = repo::read_json(sig)?;
    sign::verify(bytes, &signature, trusted)
        .wrap_err_with(|| format!("{}: verifying {}", models.display(), sig.display()))
}

fn pin(
    models: &Path,
    output: &Path,
    sig: Option<&Path>,
    trusted: &[[u8; 32]],
    allow_older: bool,
) -> Result<()> {
    let bytes = read(models)?;
    if let Some(sig) = sig {
        verify_signature(models, &bytes, sig, trusted)?;
    }
    let snapshot = checked_snapshot(models, &bytes)?;
    if !allow_older && let Some(pinned) = read_optional(output)? {
        let pinned = parse_bounded(&zstd::stream::decode_all(pinned.as_slice())?)
            .wrap_err_with(|| format!("{}: invalid pin", output.display()))?;
        if !matches!(
            snapshot.is_newer_than(&pinned),
            Ok(Ordering::Greater | Ordering::Equal)
        ) {
            bail!(
                "{}: updated_at {} {PIN_MESSAGE} {} ({}); pass --allow-older to replace it",
                models.display(),
                snapshot.updated_at,
                output.display(),
                pinned.updated_at
            );
        }
    }
    let compressed = zstd::stream::encode_all(bytes.as_slice(), ZSTD_LEVEL)?;
    let metadata = format!(
        "updated_at = \"{}\"\ncontent_sha256 = \"{}\"\nsha256 = \"{}\"\n",
        snapshot.updated_at,
        snapshot.content_sha256,
        hex(&Sha256::digest(&bytes))
    );
    write(output, &compressed)?;
    write(&append_extension(output, "toml"), metadata.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use base64::{Engine, engine::general_purpose::STANDARD};
    use clap::Parser;
    use test_case::test_case;

    use super::{
        Cli, FUTURE_MESSAGE, PIN_MESSAGE, STAMP_MESSAGE, append_extension, execute, read,
        validate_path, write_json,
    };
    use crate::{
        CatalogProvider, Provider, parse_bounded,
        repo::{self, tests::repo_fixture},
        routing::{PROTECTED_MESSAGE, UNACCEPTED_MESSAGE},
        sign::{
            self,
            tests::{TEST_PUBLIC_KEY_B64, TEST_SIGNING_KEY_PKCS8_B64},
        },
        tests::NOW,
        validate::PROVIDER_REQUIRES_MESSAGE,
    };

    const EARLIER: &str = "2026-10-28T12:00:00Z";
    const LATER: &str = "2026-10-30T12:00:00Z";
    const CLOCK: &str = "2026-10-31T00:00:00Z";
    const SKEWED: &str = "2026-10-31T00:04:00Z";
    const FUTURE: &str = "2026-10-31T00:06:00Z";
    const INVALID_PIN: &[&str] = &["caudra-models", "pin", "models.json", "models.zst"];
    const BUILTIN_ROW: &str = "catalog";
    const NEW_ROW: &str = "gateway";
    const NEW_ROW_KEY: &str = "GATEWAY_API_KEY";
    const MOVED_API: &str = "https://moved.example.com/v1";
    const REQUIREMENT: &str = "future-transport";
    const ROUTING_REASON: &str = "confirmed on the provider's page";

    fn try_command(args: &[&str], clock: &str) -> color_eyre::Result<()> {
        execute(
            Cli::try_parse_from(args).unwrap().command,
            clock.parse().unwrap(),
        )
    }

    fn command(args: &[&str]) {
        try_command(args, CLOCK).unwrap();
    }

    fn error(args: &[&str], clock: &str) -> String {
        format!("{:?}", try_command(args, clock).unwrap_err())
    }

    fn edit_provider(dir: &std::path::Path, edit: impl FnOnce(&mut Provider)) {
        let path = dir.join("providers/example.json");
        let mut provider: Provider = repo::read_json(&path).unwrap();
        edit(&mut provider);
        write_json(&path, &provider).unwrap();
    }

    #[test]
    fn build_is_reproducible_and_stamps_only_edits() {
        let dir = repo_fixture();
        let root = dir.path().to_str().unwrap();
        command(&["caudra-models", "build", root, "--now", NOW]);
        let path = dir.path().join(repo::DIST);
        let original = read(&path).unwrap();
        command(&["caudra-models", "build", root, "--now", LATER]);
        assert_eq!(read(&path).unwrap(), original);
        command(&["caudra-models", "validate", root]);
        edit_provider(dir.path(), |provider| provider.display_name.push('!'));
        assert!(validate_path(dir.path(), None, CLOCK.parse().unwrap()).is_err());
        command(&["caudra-models", "build", root, "--now", LATER]);
        let edited = parse_bounded(&read(&path).unwrap()).unwrap();
        assert_eq!(edited.updated_at, LATER.parse().unwrap());
        assert_ne!(
            edited.content_sha256,
            parse_bounded(&original).unwrap().content_sha256
        );
        command(&["caudra-models", "validate", root]);
        fs::write(dir.path().join("schema/provider.v1.json"), b"{}").unwrap();
        assert!(validate_path(dir.path(), None, CLOCK.parse().unwrap()).is_err());
    }

    #[test_case(EARLIER; "earlier")]
    #[test_case(NOW; "equal")]
    fn build_refuses_a_changed_snapshot_not_after_the_committed_one(now: &str) {
        let dir = repo_fixture();
        let root = dir.path().to_str().unwrap();
        command(&["caudra-models", "build", root, "--now", NOW]);
        command(&["caudra-models", "build", root, "--now", EARLIER]);
        edit_provider(dir.path(), |provider| provider.display_name.push('!'));
        assert!(
            error(&["caudra-models", "build", root, "--now", now], CLOCK).contains(STAMP_MESSAGE)
        );
    }

    #[test_case(SKEWED, true; "within_the_skew")]
    #[test_case(FUTURE, false; "beyond_the_skew")]
    fn build_refuses_a_time_in_the_future(now: &str, accepted: bool) {
        let dir = repo_fixture();
        let args = [
            "caudra-models",
            "build",
            dir.path().to_str().unwrap(),
            "--now",
            now,
        ];
        if accepted {
            try_command(&args, CLOCK).unwrap();
        } else {
            assert!(error(&args, CLOCK).contains(FUTURE_MESSAGE));
        }
    }

    #[test_case(false; "repository")]
    #[test_case(true; "file")]
    fn validate_flags_a_snapshot_stamped_in_the_future(file: bool) {
        let dir = repo_fixture();
        let root = dir.path().to_str().unwrap();
        try_command(&["caudra-models", "build", root, "--now", LATER], LATER).unwrap();
        let dist = dir.path().join(repo::DIST);
        let target = if file { dist.to_str().unwrap() } else { root };
        try_command(&["caudra-models", "validate", target], LATER).unwrap();
        assert!(error(&["caudra-models", "validate", target], NOW).contains(FUTURE_MESSAGE));
    }

    #[test]
    fn pin_refuses_an_older_snapshot_unless_allowed() {
        let dir = repo_fixture();
        let root = dir.path().to_str().unwrap();
        command(&["caudra-models", "build", root, "--now", NOW]);
        let older = dir.path().join("older.json");
        fs::copy(dir.path().join(repo::DIST), &older).unwrap();
        edit_provider(dir.path(), |provider| provider.display_name.push('!'));
        command(&["caudra-models", "build", root, "--now", LATER]);
        let output = dir.path().join("bundled.json.zst");
        let pin = |models: &std::path::Path, extra: &[&str]| {
            let mut args = vec![
                "caudra-models",
                "pin",
                models.to_str().unwrap(),
                output.to_str().unwrap(),
                "--unsigned",
            ];
            args.extend(extra);
            try_command(&args, CLOCK).map_err(|error| format!("{error:?}"))
        };
        let newer = dir.path().join(repo::DIST);
        pin(&newer, &[]).unwrap();
        pin(&newer, &[]).unwrap();
        assert!(pin(&older, &[]).unwrap_err().contains(PIN_MESSAGE));
        pin(&older, &["--allow-older"]).unwrap();
        pin(&newer, &[]).unwrap();
    }

    fn published_repo() -> (tempfile::TempDir, String) {
        let dir = repo_fixture();
        let root = dir.path().to_str().unwrap().to_owned();
        command(&["caudra-models", "build", &root, "--now", NOW]);
        let published = dir.path().join("published.json");
        fs::copy(dir.path().join(repo::DIST), &published).unwrap();
        (dir, root)
    }

    fn validate_published(dir: &tempfile::TempDir, root: &str) -> color_eyre::Result<()> {
        command(&["caudra-models", "build", root, "--now", LATER]);
        let published = dir.path().join("published.json");
        try_command(
            &[
                "caudra-models",
                "validate",
                root,
                "--published",
                published.to_str().unwrap(),
            ],
            CLOCK,
        )
    }

    #[test_case(BUILTIN_ROW, false, Some(PROTECTED_MESSAGE); "protected_row")]
    #[test_case(NEW_ROW, false, Some(UNACCEPTED_MESSAGE); "new_row")]
    #[test_case(BUILTIN_ROW, true, None; "accepted")]
    fn validate_needs_an_acceptance_for_each_routing_change(
        row: &str,
        accepted: bool,
        expected: Option<&str>,
    ) {
        let (dir, root) = published_repo();
        let import = dir.path().join(repo::IMPORT);
        let mut catalog: std::collections::BTreeMap<String, CatalogProvider> =
            repo::read_json(&import).unwrap();
        let mut moved = catalog[BUILTIN_ROW].clone();
        moved.api = Some(MOVED_API.to_owned());
        if row == NEW_ROW {
            moved.env = vec![NEW_ROW_KEY.to_owned()];
        }
        catalog.insert(row.to_owned(), moved);
        write_json(&import, &catalog).unwrap();
        if accepted {
            write_json(
                &dir.path().join("audit/accepted.json"),
                &serde_json::json!({"accepted": [], "caps": [], "routing": [
                    {"provider": row, "field": "api", "catalog": MOVED_API, "reason": ROUTING_REASON}
                ]}),
            )
            .unwrap();
        }
        let result = validate_published(&dir, &root).map_err(|error| format!("{error:?}"));
        match expected {
            Some(message) => assert!(result.unwrap_err().contains(message)),
            None => result.unwrap(),
        }
    }

    #[test]
    fn validate_keeps_a_published_provider_servable() {
        let (dir, root) = published_repo();
        edit_provider(dir.path(), |provider| {
            provider.requires = vec![REQUIREMENT.to_owned()];
        });
        let error = format!("{:?}", validate_published(&dir, &root).unwrap_err());
        assert!(error.contains(PROVIDER_REQUIRES_MESSAGE));
    }

    #[test_case(false; "unsigned")]
    #[test_case(true; "signed")]
    fn pin_round_trip(signed: bool) {
        let dir = repo_fixture();
        command(&[
            "caudra-models",
            "build",
            dir.path().to_str().unwrap(),
            "--now",
            NOW,
        ]);
        let models = dir.path().join(repo::DIST);
        let output = dir.path().join("models.json.zst");
        let bytes = read(&models).unwrap();
        let signature = append_extension(&models, "sig");
        write_json(
            &signature,
            &sign::sign(
                &bytes,
                &STANDARD.decode(TEST_SIGNING_KEY_PKCS8_B64).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let mut args = vec![
            "caudra-models",
            "pin",
            models.to_str().unwrap(),
            output.to_str().unwrap(),
        ];
        if signed {
            args.extend([
                "--sig",
                signature.to_str().unwrap(),
                "--key",
                TEST_PUBLIC_KEY_B64,
            ]);
        } else {
            args.push("--unsigned");
        }
        command(&args);
        assert_eq!(
            zstd::stream::decode_all(read(&output).unwrap().as_slice()).unwrap(),
            bytes
        );
        let snapshot = parse_bounded(&bytes).unwrap();
        let metadata = fs::read_to_string(append_extension(&output, "toml")).unwrap();
        assert!(metadata.contains(&snapshot.content_sha256));
        assert!(metadata.contains(NOW));
        if signed {
            fs::write(&models, [bytes.as_slice(), b" "].concat()).unwrap();
            assert!(try_command(&args, CLOCK).is_err());
        }
    }

    #[test]
    fn pin_requires_explicit_signature_policy() {
        assert!(Cli::try_parse_from(INVALID_PIN).is_err());
        let mut args = INVALID_PIN.to_vec();
        args.extend(["--unsigned", "--sig", "models.json.sig"]);
        assert!(Cli::try_parse_from(args).is_err());
    }
}
