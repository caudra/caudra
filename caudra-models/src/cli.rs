use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Args, Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use jiff::Timestamp;
use schemars::schema_for;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    Provider, Snapshot, TRUSTED_KEYS, hex, import_models_dev, parse_bounded,
    repo::{self, DecisionFile},
    sign, to_pretty_bytes, validate,
};

const SIGNING_KEY_ENV: &str = "CAUDRA_MODELS_SIGNING_KEY";
const ZSTD_LEVEL: i32 = 19;
const HASH_MESSAGE: &str = "content hash does not match payload";
const PROVIDER_SCHEMA: &str = "provider.v1.json";
const MODELS_SCHEMA: &str = "models.v1.json";
const DECISIONS_SCHEMA: &str = "decision-models.v1.json";

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
    /// Validate a snapshot or check a repository's generated files.
    Validate { path: PathBuf },
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
    execute(Cli::parse().command)
}

fn execute(command: Command) -> Result<()> {
    match command {
        Command::Validate { path } => validate_path(&path),
        Command::Build { repo, now } => build(&repo, now.unwrap_or_else(Timestamp::now)),
        Command::ImportModelsDev { api, repo } => {
            let slugs = repo::load_providers(&repo)?
                .into_iter()
                .filter_map(|provider| provider.catalog_slug)
                .collect();
            let imported = import_models_dev(&read(&api)?, &slugs)
                .wrap_err_with(|| format!("{}: invalid catalog JSON", api.display()))?;
            write_json(&repo.join(repo::IMPORT), &imported)
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
            keys,
        } => pin(&models, &output, sig.as_deref(), &keys.decode()?),
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

fn artifacts(dir: &Path, now: Timestamp) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let path = dir.join(repo::DIST);
    let previous = match fs::read(&path) {
        Ok(bytes) => Some(checked_snapshot(&path, &bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).wrap_err_with(|| format!("reading {}", path.display())),
    };
    let mut snapshot = Snapshot::stamp(repo::load(dir)?, previous.as_ref(), now);
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
    Ok(artifacts)
}

fn build(dir: &Path, now: Timestamp) -> Result<()> {
    for (path, bytes) in artifacts(dir, now)? {
        write(&path, &bytes)?;
    }
    Ok(())
}

fn validate_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        let existing = path.join(repo::DIST);
        let previous = checked_snapshot(&existing, &read(&existing)?)?;
        for (file, expected) in artifacts(path, previous.updated_at)? {
            if read(&file)? != expected {
                bail!(
                    "{}: generated file is stale; run build {}",
                    file.display(),
                    path.display()
                );
            }
        }
    } else {
        checked_snapshot(path, &read(path)?)?;
    }
    Ok(())
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

fn pin(models: &Path, output: &Path, sig: Option<&Path>, trusted: &[[u8; 32]]) -> Result<()> {
    let bytes = read(models)?;
    if let Some(sig) = sig {
        verify_signature(models, &bytes, sig, trusted)?;
    }
    let snapshot = checked_snapshot(models, &bytes)?;
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

    use super::{Cli, append_extension, execute, read, validate_path, write_json};
    use crate::{
        Provider, parse_bounded,
        repo::{self, tests::repo_fixture},
        sign::{
            self,
            tests::{TEST_PUBLIC_KEY_B64, TEST_SIGNING_KEY_PKCS8_B64},
        },
        tests::NOW,
    };

    const LATER: &str = "2026-10-30T12:00:00Z";
    const INVALID_PIN: &[&str] = &["caudra-models", "pin", "models.json", "models.zst"];

    fn command(args: &[&str]) {
        execute(Cli::try_parse_from(args).unwrap().command).unwrap();
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
        let provider_path = dir.path().join("providers/example.json");
        let mut provider: Provider = repo::read_json(&provider_path).unwrap();
        provider.display_name.push('!');
        write_json(&provider_path, &provider).unwrap();
        assert!(validate_path(dir.path()).is_err());
        command(&["caudra-models", "build", root, "--now", LATER]);
        let edited = parse_bounded(&read(&path).unwrap()).unwrap();
        assert_eq!(edited.updated_at, LATER.parse().unwrap());
        assert_ne!(
            edited.content_sha256,
            parse_bounded(&original).unwrap().content_sha256
        );
        command(&["caudra-models", "validate", root]);
        fs::write(dir.path().join("schema/provider.v1.json"), b"{}").unwrap();
        assert!(validate_path(dir.path()).is_err());
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
            assert!(execute(Cli::try_parse_from(&args).unwrap().command).is_err());
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
