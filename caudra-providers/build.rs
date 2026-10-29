use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use caudra_models::{SUPPORTED_REQUIREMENTS, parse_bounded, validate};

const BUNDLED: &str = "../models/bundled.json";
const COMPRESSED: &str = "bundled.json.zst";
const COMPRESSION_LEVEL: i32 = 19;

fn main() {
    println!("cargo:rerun-if-changed={BUNDLED}");
    if let Err(error) = compress_bundled() {
        eprintln!("{error}");
        std::process::exit(1);
    }
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_SHORT_HASH={hash}");
}

fn compress_bundled() -> Result<(), String> {
    let bytes = fs::read(BUNDLED).map_err(|error| format!("{BUNDLED}: {error}"))?;
    let snapshot = parse_bounded(&bytes).map_err(|error| format!("{BUNDLED}: {error}"))?;
    validate(&snapshot).map_err(|issues| {
        issues
            .iter()
            .map(|issue| format!("{BUNDLED}: {issue}"))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if !snapshot.hash_matches() {
        return Err(format!(
            "{BUNDLED}: /content_sha256: content hash does not match payload"
        ));
    }
    for (index, provider) in snapshot.payload.providers.iter().enumerate() {
        check_requirements(&provider.requires, &format!("/providers/{index}"))?;
        for (model_index, model) in provider.models.iter().enumerate() {
            check_requirements(
                &model.requires,
                &format!("/providers/{index}/models/{model_index}"),
            )?;
        }
    }
    let compressed = zstd::encode_all(bytes.as_slice(), COMPRESSION_LEVEL)
        .map_err(|error| format!("{BUNDLED}: {error}"))?;
    let out_dir = env::var("OUT_DIR").map_err(|error| format!("OUT_DIR: {error}"))?;
    let out = Path::new(&out_dir).join(COMPRESSED);
    fs::write(&out, compressed).map_err(|error| format!("{}: {error}", out.display()))
}

fn check_requirements(requirements: &[String], pointer: &str) -> Result<(), String> {
    for (index, requirement) in requirements.iter().enumerate() {
        if !SUPPORTED_REQUIREMENTS.contains(&requirement.as_str()) {
            return Err(format!(
                "{BUNDLED}: {pointer}/requires/{index}: unsupported requirement {requirement:?}"
            ));
        }
    }
    Ok(())
}
