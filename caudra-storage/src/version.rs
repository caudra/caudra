use std::cmp::Ordering;
use std::collections::HashSet;
use std::env::consts::{ARCH, OS};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use isahc::Request;
use isahc::config::{Configurable, VersionNegotiation};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{StorageError, atomic_write, paths, try_exclusive_state_lock};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/caudra/caudra/releases";
const DOWNLOAD_URL: &str = "https://github.com/caudra/caudra/releases/download";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(60 * 60);
const PAGE_SIZE: usize = 100;
const MAX_PAGES: u32 = 10;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 4096;
const CACHE_SCHEMA: u32 = 2;
const CACHE_DIRECTORY: &str = "release-checks";
const LOCK_MODE: u32 = 0o600;
const CHECKSUM_ASSET: &str = "sha256sums.txt";
const UNIX_INSTALLER: &str = "install.sh";
const WINDOWS_INSTALLER: &str = "install.ps1";
const UPLOADED: &str = "uploaded";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    #[default]
    Auto,
    Stable,
    Preview,
}

impl UpdateChannel {
    pub fn resolve(self) -> Result<Self, VersionError> {
        self.resolve_for(CURRENT)
    }

    fn resolve_for(self, current: &str) -> Result<Self, VersionError> {
        match self {
            Self::Auto => {
                let version = Version::parse(current)?;
                Ok(if version.pre.is_empty() {
                    Self::Stable
                } else {
                    Self::Preview
                })
            }
            channel => Ok(channel),
        }
    }
}

impl fmt::Display for UpdateChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Stable => "stable",
            Self::Preview => "preview",
        })
    }
}

impl FromStr for UpdateChannel {
    type Err = VersionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "stable" => Ok(Self::Stable),
            "preview" => Ok(Self::Preview),
            _ => Err(VersionError::InvalidChannel(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: String,
}

impl Release {
    pub fn installer_url(&self, windows: bool) -> String {
        let installer = installer_name(windows);
        format!("{DOWNLOAD_URL}/{}/{installer}", self.tag)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] isahc::Error),
    #[error("failed to build request: {0}")]
    Request(#[from] isahc::http::Error),
    #[error("failed to read response: {0}")]
    Io(#[from] io::Error),
    #[error("server returned HTTP {0}")]
    Status(u16),
    #[error("invalid response: {0}")]
    InvalidResponse(&'static str),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid semantic version: {0}")]
    Semver(#[from] semver::Error),
    #[error("unknown update channel {0:?}; expected auto, stable, or preview")]
    InvalidChannel(String),
    #[error("updates are not supported for {os}/{arch}")]
    UnsupportedTarget {
        os: &'static str,
        arch: &'static str,
    },
    #[error("no published {channel} release is available for {target}")]
    NoEligibleRelease {
        channel: UpdateChannel,
        target: String,
    },
    #[error(
        "newest release {tag} lacks unique, uploaded, positive-size canonical assets for {target}"
    )]
    IncompleteRelease { tag: String, target: String },
    #[error("release discovery returned duplicate tag {0}; refusing an ambiguous result")]
    DuplicateTag(String),
    #[error("release discovery exceeded its pagination limit; refusing an incomplete result")]
    PaginationLimit,
    #[error("release discovery exceeded its time limit")]
    Deadline,
    #[error("response exceeds the {0}-byte limit")]
    ResponseTooLarge(usize),
    #[error("release cache failed: {0}")]
    Cache(#[from] StorageError),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((Version::parse(latest), Version::parse(current)),
        (Ok(latest), Ok(current)) if latest.cmp_precedence(&current).is_gt())
}

pub fn current_target() -> Result<&'static str, VersionError> {
    target_for(OS, ARCH)
}

fn target_for(os: &'static str, arch: &'static str) -> Result<&'static str, VersionError> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64" | "aarch64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(VersionError::UnsupportedTarget { os, arch }),
    }
}

fn installer_name(windows: bool) -> &'static str {
    if windows {
        WINDOWS_INSTALLER
    } else {
        UNIX_INSTALLER
    }
}

fn tag_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v')?).ok()
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    state: Option<String>,
    size: Option<u64>,
    browser_download_url: Option<String>,
}

impl ApiRelease {
    fn eligible_version(&self, channel: &UpdateChannel) -> Option<Version> {
        let version = tag_version(&self.tag_name)?;
        if self.draft
            || self
                .published_at
                .as_deref()?
                .parse::<jiff::Timestamp>()
                .is_err()
            || self.prerelease == version.pre.is_empty()
            || (*channel == UpdateChannel::Stable && self.prerelease)
        {
            return None;
        }
        Some(version)
    }

    fn has_required_assets(&self, target: &str) -> bool {
        let windows = target.ends_with("-windows-msvc");
        let extension = if windows { "zip" } else { "tar.gz" };
        let archive = format!("caudra-{}-{target}.{extension}", self.tag_name);
        [archive.as_str(), CHECKSUM_ASSET, installer_name(windows)]
            .iter()
            .all(|name| {
                let mut matching = self.assets.iter().filter(|asset| asset.name == *name);
                let Some(asset) = matching.next() else {
                    return false;
                };
                matching.next().is_none()
                    && asset.state.as_deref() == Some(UPLOADED)
                    && asset.size.is_some_and(|size| size > 0)
                    && asset.browser_download_url.as_deref()
                        == Some(format!("{DOWNLOAD_URL}/{}/{name}", self.tag_name).as_str())
            })
    }
}

fn discover(
    channel: UpdateChannel,
    target: &str,
    mut fetch_page: impl FnMut(u32) -> Result<Vec<u8>, VersionError>,
) -> Result<Release, VersionError> {
    let mut best: Option<(Version, ApiRelease)> = None;
    let mut seen = HashSet::new();
    for page in 1..=MAX_PAGES {
        let bytes = fetch_page(page)?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(VersionError::ResponseTooLarge(MAX_RESPONSE_BYTES));
        }
        let releases: Vec<ApiRelease> = serde_json::from_slice(&bytes)?;
        if releases.len() > PAGE_SIZE {
            return Err(VersionError::InvalidResponse("too many releases in a page"));
        }
        let complete = releases.is_empty();
        for release in releases {
            if !seen.insert(release.tag_name.clone()) {
                return Err(VersionError::DuplicateTag(release.tag_name));
            }
            let Some(version) = release.eligible_version(&channel) else {
                continue;
            };
            if best.as_ref().is_none_or(|(selected, previous)| {
                match version.cmp_precedence(selected) {
                    Ordering::Greater => true,
                    Ordering::Equal => release.tag_name < previous.tag_name,
                    Ordering::Less => false,
                }
            }) {
                best = Some((version, release));
            }
        }
        if complete {
            let (version, release) = best.ok_or_else(|| VersionError::NoEligibleRelease {
                channel,
                target: target.to_owned(),
            })?;
            if !release.has_required_assets(target) {
                return Err(VersionError::IncompleteRelease {
                    tag: release.tag_name,
                    target: target.to_owned(),
                });
            }
            return Ok(Release {
                tag: release.tag_name,
                version: version.to_string(),
            });
        }
    }
    Err(VersionError::PaginationLimit)
}

fn remaining(started: Instant) -> Result<Duration, VersionError> {
    DISCOVERY_TIMEOUT
        .checked_sub(started.elapsed())
        .filter(|duration| !duration.is_zero())
        .ok_or(VersionError::Deadline)
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, VersionError> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(VersionError::ResponseTooLarge(limit));
    }
    Ok(bytes)
}

fn fetch_page(
    client: &isahc::HttpClient,
    url: &str,
    timeout: Duration,
) -> Result<Vec<u8>, VersionError> {
    let request = Request::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "caudra")
        .timeout(timeout.min(REQUEST_TIMEOUT))
        .body(())?;
    let response = client.send(request)?;
    if !response.status().is_success() {
        return Err(VersionError::Status(response.status().as_u16()));
    }
    read_bounded(response.into_body(), MAX_RESPONSE_BYTES)
}

pub fn fetch_release(channel: UpdateChannel) -> Result<Release, VersionError> {
    let channel = channel.resolve()?;
    let target = current_target()?;
    let started = Instant::now();
    let client = isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .version_negotiation(VersionNegotiation::http11())
        .build()?;
    let release = discover(channel, target, |page| {
        let url = format!("{RELEASES_URL}?per_page={PAGE_SIZE}&page={page}");
        fetch_page(&client, &url, remaining(started)?).or_else(|error| {
            if !matches!(error, VersionError::Http(_)) {
                return Err(error);
            }
            curl_fetch_bounded(&url, remaining(started)?.min(REQUEST_TIMEOUT)).map_err(|_| error)
        })
    })?;
    remaining(started)?;
    Ok(release)
}

pub fn curl_fetch(url: &str) -> io::Result<Vec<u8>> {
    curl_fetch_bounded(url, REQUEST_TIMEOUT)
}

fn curl_fetch_bounded(url: &str, timeout: Duration) -> io::Result<Vec<u8>> {
    let mut child = Command::new("curl")
        .args([
            "--disable",
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
        ])
        .args(["--max-redirs", "3", "-A", "caudra"])
        .args(["-H", "Accept: application/vnd.github+json"])
        .arg("--connect-timeout")
        .arg(CONNECT_TIMEOUT.min(timeout).as_secs_f64().to_string())
        .arg("--max-time")
        .arg(timeout.as_secs_f64().to_string())
        .arg("--max-filesize")
        .arg(MAX_RESPONSE_BYTES.to_string())
        .arg("--url")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = match child.stdout.take() {
        Some(stdout) => read_bounded(stdout, MAX_RESPONSE_BYTES).map_err(io::Error::other),
        None => Err(io::Error::other("curl stdout is unavailable")),
    };
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let bytes = result?;
    if !status.success() {
        return Err(io::Error::other(format!("curl failed with {status}")));
    }
    Ok(bytes)
}

#[derive(Deserialize, Serialize)]
struct CacheEntry {
    schema: u32,
    channel: UpdateChannel,
    target: String,
    checked_at: u64,
    release: Option<Release>,
}

impl CacheEntry {
    fn usable(&self, channel: &UpdateChannel, target: &str, now: u64) -> bool {
        if self.schema != CACHE_SCHEMA || self.channel != *channel || self.target != target {
            return false;
        }
        let Some(age) = now.checked_sub(self.checked_at) else {
            return false;
        };
        let ttl = if let Some(release) = &self.release {
            let Some(version) = tag_version(&release.tag) else {
                return false;
            };
            if version.to_string() != release.version
                || (*channel == UpdateChannel::Stable && !version.pre.is_empty())
            {
                return false;
            }
            CACHE_TTL
        } else {
            FAILURE_COOLDOWN
        };
        age < ttl.as_secs()
    }
}

pub fn fetch_release_cached(channel: UpdateChannel) -> Result<Option<Release>, VersionError> {
    let channel = channel.resolve()?;
    let target = current_target()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| VersionError::Clock)?
        .as_secs();
    cached_at(
        &paths::cache_dir()?.join(CACHE_DIRECTORY),
        channel.clone(),
        target,
        now,
        || fetch_release(channel),
    )
}

fn cached_at(
    directory: &Path,
    channel: UpdateChannel,
    target: &str,
    now: u64,
    fetch: impl FnOnce() -> Result<Release, VersionError>,
) -> Result<Option<Release>, VersionError> {
    fs::create_dir_all(directory)?;
    let key = format!("{channel}-{target}");
    let path = directory.join(format!("{key}.json"));
    let Some(_lock) = try_exclusive_state_lock(&directory.join(format!("{key}.lock")), LOCK_MODE)?
    else {
        return Ok(None);
    };
    if let Some(entry) = File::open(&path)
        .ok()
        .and_then(|file| read_bounded(file, MAX_CACHE_BYTES).ok())
        .and_then(|bytes| serde_json::from_slice::<CacheEntry>(&bytes).ok())
        .filter(|entry| entry.usable(&channel, target, now))
    {
        return Ok(entry.release);
    }
    let result = fetch();
    let entry = CacheEntry {
        schema: CACHE_SCHEMA,
        channel,
        target: target.to_owned(),
        checked_at: now,
        release: result.as_ref().ok().cloned(),
    };
    let write = atomic_write(&path, &serde_json::to_vec(&entry)?);
    match result {
        Ok(release) => {
            write?;
            Ok(Some(release))
        }
        Err(error) => {
            if let Err(cache_error) = write {
                tracing::debug!(error = %cache_error, "could not cache failed release check");
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::time::Instant;

    use serde_json::{Value, json};
    use tempfile::tempdir;
    use test_case::test_case;

    use super::{
        CACHE_SCHEMA, CACHE_TTL, CHECKSUM_ASSET, CacheEntry, DISCOVERY_TIMEOUT, DOWNLOAD_URL,
        FAILURE_COOLDOWN, LOCK_MODE, MAX_CACHE_BYTES, MAX_PAGES, MAX_RESPONSE_BYTES, PAGE_SIZE,
        Release, UNIX_INSTALLER, UPLOADED, UpdateChannel, VersionError, WINDOWS_INSTALLER,
        cached_at, discover, is_newer, read_bounded, remaining, tag_version, target_for,
    };
    use crate::try_exclusive_state_lock;

    const LINUX: &str = "x86_64-unknown-linux-musl";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";
    const TAG: &str = "v1.2.0";
    const VERSION: &str = "1.2.0";
    const NOW: u64 = 1_800_000_000;
    const PUBLISHED: &str = "2026-01-01T00:00:00Z";
    const UNAVAILABLE: u16 = 503;

    fn release() -> Release {
        Release {
            tag: TAG.to_owned(),
            version: VERSION.to_owned(),
        }
    }

    fn fixture(tag: &str, target: &str) -> Value {
        let windows = target == WINDOWS;
        let extension = if windows { "zip" } else { "tar.gz" };
        let archive = format!("caudra-{tag}-{target}.{extension}");
        let assets: Vec<_> = [
            archive.as_str(),
            CHECKSUM_ASSET,
            if windows {
                WINDOWS_INSTALLER
            } else {
                UNIX_INSTALLER
            },
        ]
        .iter()
        .map(|name| {
            json!({
                "name": name, "state": UPLOADED, "size": 1,
                "browser_download_url": format!("{DOWNLOAD_URL}/{tag}/{name}"),
            })
        })
        .collect();
        json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": tag_version(tag).is_some_and(|version| !version.pre.is_empty()),
            "published_at": PUBLISHED,
            "assets": assets,
        })
    }

    fn full_page(page: u32) -> Vec<Value> {
        (0..PAGE_SIZE)
            .map(|patch| fixture(&format!("v0.{page}.{patch}"), LINUX))
            .collect()
    }

    fn select(
        channel: UpdateChannel,
        target: &str,
        releases: &[Value],
    ) -> Result<Release, VersionError> {
        discover(channel, target, |page| {
            Ok(serde_json::to_vec(if page == 1 { releases } else { &[] }).unwrap())
        })
    }

    #[test_case("0.2.0", "0.1.0", true; "minor_bump")]
    #[test_case("1.0.0", "0.9.9", true; "major_bump")]
    #[test_case("0.1.1", "0.1.0", true; "patch_bump")]
    #[test_case("0.1.0", "0.1.0", false; "equal")]
    #[test_case("0.0.9", "0.1.0", false; "older")]
    #[test_case("garbage", "0.1.0", false; "invalid_latest")]
    #[test_case("1.0.0", "garbage", false; "invalid_current")]
    #[test_case("1.0.0-rc.2", "1.0.0-rc.1", true; "preview_bump")]
    #[test_case("1.0.0-rc.10", "1.0.0-rc.2", true; "numeric_prerelease")]
    #[test_case("1.0.0", "1.0.0-rc.1", true; "promotion")]
    #[test_case("1.0.0-rc.1", "1.0.0", false; "prerelease_older_than_stable")]
    #[test_case("1.0.0-rc.1", "0.9.0", true; "next_preview")]
    #[test_case("1.0.0+z", "1.0.0+a", false; "build_metadata_not_precedence")]
    #[test_case("1.0.0.1", "1.0.0", false; "extra_component")]
    #[test_case("01.0.0", "0.1.0", false; "leading_zero")]
    fn semantic_precedence(latest: &str, current: &str, expected: bool) {
        assert_eq!(is_newer(latest, current), expected);
    }

    #[test_case("auto", UpdateChannel::Auto)]
    #[test_case("stable", UpdateChannel::Stable)]
    #[test_case("preview", UpdateChannel::Preview)]
    fn channel_roundtrip(text: &str, expected: UpdateChannel) {
        assert_eq!(text.parse::<UpdateChannel>().unwrap(), expected);
        assert_eq!(expected.to_string(), text);
        assert_eq!(serde_json::to_value(&expected).unwrap(), json!(text));
        assert_eq!(
            serde_json::from_value::<UpdateChannel>(json!(text)).unwrap(),
            expected
        );
    }

    #[test_case("nightly")]
    #[test_case("Stable")]
    #[test_case(" preview ")]
    fn invalid_channels(text: &str) {
        assert!(text.parse::<UpdateChannel>().is_err());
        assert!(serde_json::from_value::<UpdateChannel>(json!(text)).is_err());
    }

    #[test_case("1.0.0", UpdateChannel::Stable)]
    #[test_case("1.0.0+build", UpdateChannel::Stable)]
    #[test_case("1.0.0-rc.1", UpdateChannel::Preview)]
    fn auto_channel(current: &str, expected: UpdateChannel) {
        assert_eq!(
            UpdateChannel::default().resolve_for(current).unwrap(),
            expected
        );
        assert_eq!(
            UpdateChannel::Stable.resolve_for(current).unwrap(),
            UpdateChannel::Stable
        );
        assert_eq!(
            UpdateChannel::Preview.resolve_for(current).unwrap(),
            UpdateChannel::Preview
        );
    }

    #[test_case(UpdateChannel::Stable, "v1.10.0")]
    #[test_case(UpdateChannel::Preview, "v2.0.0-rc.10")]
    fn unordered_release_selection(channel: UpdateChannel, expected: &str) {
        let mut releases: Vec<_> = ["v1.9.0", "v2.0.0-rc.2", "v1.10.0", "v2.0.0-rc.10"]
            .iter()
            .map(|tag| fixture(tag, LINUX))
            .collect();
        assert_eq!(
            select(channel.clone(), LINUX, &releases).unwrap().tag,
            expected
        );
        releases.reverse();
        assert_eq!(select(channel, LINUX, &releases).unwrap().tag, expected);
    }

    #[test_case(UpdateChannel::Stable)]
    #[test_case(UpdateChannel::Preview)]
    fn stable_promotion_and_metadata_ties(channel: UpdateChannel) {
        let mut releases: Vec<_> = ["v1.2.0+z", "v1.2.0-rc.1", TAG, "v1.2.0+a"]
            .iter()
            .map(|tag| fixture(tag, LINUX))
            .collect();
        assert_eq!(
            select(channel.clone(), LINUX, &releases).unwrap(),
            release()
        );
        releases.reverse();
        assert_eq!(select(channel, LINUX, &releases).unwrap(), release());
    }

    #[test_case("1.2.0")]
    #[test_case("V1.2.0")]
    #[test_case("v01.2.0")]
    #[test_case("v1.2")]
    #[test_case("v1.2.0.0")]
    #[test_case("v1.2.0-01")]
    #[test_case("v1.2.0/evil")]
    #[test_case("v1.2.0\n")]
    fn invalid_tags_are_ineligible(tag: &str) {
        assert!(matches!(
            select(UpdateChannel::Preview, LINUX, &[fixture(tag, LINUX)]),
            Err(VersionError::NoEligibleRelease { .. })
        ));
    }

    #[test_case("draft", json!(true))]
    #[test_case("published_at", Value::Null)]
    #[test_case("published_at", json!("invalid"))]
    #[test_case("prerelease", json!(true))]
    fn invalid_release_metadata(field: &str, value: Value) {
        let mut invalid = fixture(TAG, LINUX);
        invalid[field] = value;
        assert!(matches!(
            select(UpdateChannel::Preview, LINUX, &[invalid]),
            Err(VersionError::NoEligibleRelease { .. })
        ));
    }

    #[test_case(false)]
    #[test_case(true)]
    fn prerelease_marker_must_match_tag(stable_channel: bool) {
        let mut preview = fixture("v2.0.0-rc.1", LINUX);
        preview["prerelease"] = json!(false);
        let channel = if stable_channel {
            UpdateChannel::Stable
        } else {
            UpdateChannel::Preview
        };
        assert!(matches!(
            select(channel, LINUX, &[preview]),
            Err(VersionError::NoEligibleRelease { .. })
        ));
    }

    #[test_case(LINUX, 0; "unix_archive")]
    #[test_case(LINUX, 1; "unix_checksums")]
    #[test_case(LINUX, 2; "unix_installer")]
    #[test_case(WINDOWS, 0; "windows_archive")]
    #[test_case(WINDOWS, 1; "windows_checksums")]
    #[test_case(WINDOWS, 2; "windows_installer")]
    fn each_platform_asset_is_required(target: &str, missing: usize) {
        let mut incomplete = fixture(TAG, target);
        incomplete["assets"].as_array_mut().unwrap().remove(missing);
        assert!(matches!(
            select(UpdateChannel::Stable, target, &[incomplete]),
            Err(VersionError::IncompleteRelease { .. })
        ));
        assert_eq!(
            select(UpdateChannel::Stable, target, &[fixture(TAG, target)]).unwrap(),
            release()
        );
    }

    #[test_case(LINUX, WINDOWS)]
    #[test_case(WINDOWS, LINUX)]
    fn wrong_platform_assets_are_ineligible(target: &str, assets_target: &str) {
        assert!(matches!(
            select(
                UpdateChannel::Stable,
                target,
                &[fixture(TAG, assets_target)]
            ),
            Err(VersionError::IncompleteRelease { .. })
        ));
    }

    #[test_case("state", json!("new"); "upload_incomplete")]
    #[test_case("state", Value::Null; "missing_upload_state")]
    #[test_case("size", json!(0); "empty_asset")]
    #[test_case("size", Value::Null; "missing_size")]
    #[test_case("browser_download_url", Value::Null; "missing_url")]
    #[test_case("browser_download_url", json!("https://example.invalid/install.sh"); "untrusted_url")]
    #[test_case("browser_download_url", json!(format!("{DOWNLOAD_URL}/v0.1.0/install.sh")); "wrong_tag_url")]
    fn required_asset_metadata_is_validated(field: &str, value: Value) {
        for target in [LINUX, WINDOWS] {
            for index in 0..3 {
                let mut invalid = fixture(TAG, target);
                invalid["assets"][index][field] = value.clone();
                assert!(matches!(select(UpdateChannel::Stable, target, &[invalid]),
                    Err(VersionError::IncompleteRelease { tag, .. }) if tag == TAG));
            }
        }
    }

    #[test_case(json!("1"); "string_size")]
    #[test_case(json!(true); "boolean_size")]
    #[test_case(json!(-1); "negative_size")]
    #[test_case(json!(1.5); "fractional_size")]
    fn invalid_asset_size_types_fail_closed(size: Value) {
        let mut invalid = fixture(TAG, LINUX);
        invalid["assets"][0]["size"] = size;
        assert!(matches!(
            select(UpdateChannel::Stable, LINUX, &[invalid]),
            Err(VersionError::Json(_))
        ));
    }

    #[test_case(0; "archive")]
    #[test_case(1; "checksums")]
    #[test_case(2; "installer")]
    fn duplicate_required_assets_fail_closed(index: usize) {
        for target in [LINUX, WINDOWS] {
            let mut invalid = fixture(TAG, target);
            let duplicate = invalid["assets"][index].clone();
            invalid["assets"].as_array_mut().unwrap().push(duplicate);
            assert!(matches!(
                select(UpdateChannel::Stable, target, &[invalid]),
                Err(VersionError::IncompleteRelease { .. })
            ));
        }
    }

    #[test_case(UpdateChannel::Stable, "v2.0.0")]
    #[test_case(UpdateChannel::Preview, "v2.0.0-rc.1")]
    fn newest_incomplete_release_does_not_fall_back(channel: UpdateChannel, tag: &str) {
        let mut incomplete = fixture(tag, LINUX);
        incomplete["assets"] = json!([]);
        let mut releases = [fixture(TAG, LINUX), incomplete];
        for _ in 0..2 {
            let result = discover(channel.clone(), LINUX, |page| {
                let release = releases.get(page as usize - 1);
                Ok(serde_json::to_vec(&release.into_iter().collect::<Vec<_>>()).unwrap())
            });
            assert!(
                matches!(result, Err(VersionError::IncompleteRelease { tag: selected, .. }) if selected == tag)
            );
            releases.reverse();
        }
    }

    #[test]
    fn incomplete_older_or_other_channel_release_does_not_block_stable() {
        let mut older = fixture("v1.0.0", LINUX);
        let mut preview = fixture("v2.0.0-rc.1", LINUX);
        older["assets"] = json!([]);
        preview["assets"] = json!([]);
        assert_eq!(
            select(
                UpdateChannel::Stable,
                LINUX,
                &[older, preview, fixture(TAG, LINUX)]
            )
            .unwrap(),
            release()
        );
    }

    #[test_case(false; "same_page")]
    #[test_case(true; "across_short_pages")]
    fn duplicate_release_tags_fail_closed(across_pages: bool) {
        let result = discover(UpdateChannel::Stable, LINUX, |page| {
            let count = match (across_pages, page) {
                (true, 1 | 2) => 1,
                (false, 1) => 2,
                _ => 0,
            };
            Ok(serde_json::to_vec(&vec![fixture(TAG, LINUX); count]).unwrap())
        });
        assert!(matches!(result, Err(VersionError::DuplicateTag(tag)) if tag == TAG));
    }

    #[test]
    fn pagination_searches_every_page() {
        let first = full_page(1);
        let second = vec![fixture("v2.0.0", LINUX)];
        let mut pages = Vec::new();
        let selected = discover(UpdateChannel::Stable, LINUX, |page| {
            pages.push(page);
            Ok(serde_json::to_vec(match page {
                1 => first.as_slice(),
                2 => second.as_slice(),
                _ => &[],
            })
            .unwrap())
        })
        .unwrap();
        assert_eq!(pages, [1, 2, 3]);
        assert_eq!(selected.tag, "v2.0.0");
    }

    #[test]
    fn pagination_exhaustion_discards_partial_result() {
        let mut calls = 0;
        let result = discover(UpdateChannel::Stable, LINUX, |page| {
            calls += 1;
            Ok(serde_json::to_vec(&full_page(page)).unwrap())
        });
        assert!(matches!(result, Err(VersionError::PaginationLimit)));
        assert_eq!(calls, MAX_PAGES);
    }

    #[test_case(false; "transport_failure")]
    #[test_case(true; "invalid_json")]
    fn later_page_failure_discards_partial_result(invalid_json: bool) {
        let result = discover(UpdateChannel::Stable, LINUX, |page| {
            if page == 1 {
                Ok(serde_json::to_vec(&full_page(page)).unwrap())
            } else if invalid_json {
                Ok(b"{".to_vec())
            } else {
                Err(VersionError::Status(UNAVAILABLE))
            }
        });
        if invalid_json {
            assert!(matches!(result, Err(VersionError::Json(_))));
        } else {
            assert!(matches!(result, Err(VersionError::Status(UNAVAILABLE))));
        }
    }

    #[test]
    fn discovery_bounds_and_empty_results() {
        assert!(matches!(
            select(UpdateChannel::Stable, LINUX, &[]),
            Err(VersionError::NoEligibleRelease { .. })
        ));
        assert!(matches!(
            select(
                UpdateChannel::Stable,
                LINUX,
                &vec![fixture(TAG, LINUX); PAGE_SIZE + 1]
            ),
            Err(VersionError::InvalidResponse(_))
        ));
        assert!(matches!(
            discover(UpdateChannel::Stable, LINUX, |_| Ok(vec![
                b' ';
                MAX_RESPONSE_BYTES
                    + 1
            ])),
            Err(VersionError::ResponseTooLarge(MAX_RESPONSE_BYTES))
        ));
        assert!(matches!(
            remaining(Instant::now() - DISCOVERY_TIMEOUT),
            Err(VersionError::Deadline)
        ));
        let data = [b'x'; MAX_CACHE_BYTES + 1];
        assert!(matches!(
            read_bounded(data.as_slice(), MAX_CACHE_BYTES),
            Err(VersionError::ResponseTooLarge(MAX_CACHE_BYTES))
        ));
        assert_eq!(
            read_bounded(&data[..MAX_CACHE_BYTES], MAX_CACHE_BYTES)
                .unwrap()
                .len(),
            MAX_CACHE_BYTES
        );
    }

    #[test_case("linux", "x86_64", LINUX)]
    #[test_case("linux", "aarch64", "aarch64-unknown-linux-musl")]
    #[test_case("macos", "x86_64", "x86_64-apple-darwin")]
    #[test_case("macos", "aarch64", "aarch64-apple-darwin")]
    #[test_case("windows", "x86_64", WINDOWS)]
    #[test_case("windows", "aarch64", WINDOWS)]
    fn supported_targets(os: &'static str, arch: &'static str, expected: &str) {
        assert_eq!(target_for(os, arch).unwrap(), expected);
    }

    #[test_case("windows", "x86")]
    #[test_case("linux", "riscv64")]
    #[test_case("freebsd", "x86_64")]
    fn unsupported_targets(os: &'static str, arch: &'static str) {
        assert!(matches!(
            target_for(os, arch),
            Err(VersionError::UnsupportedTarget { .. })
        ));
    }

    #[test_case(false, UNIX_INSTALLER)]
    #[test_case(true, WINDOWS_INSTALLER)]
    fn pinned_installer_url(windows: bool, installer: &str) {
        assert_eq!(
            release().installer_url(windows),
            format!("{DOWNLOAD_URL}/{TAG}/{installer}")
        );
    }

    #[test]
    fn successful_cache_expires_without_sliding_ttl() {
        let directory = tempdir().unwrap();
        let calls = Cell::new(0);
        let fetch = || {
            calls.set(calls.get() + 1);
            Ok(release())
        };
        for now in [NOW, NOW + CACHE_TTL.as_secs() - 1] {
            assert_eq!(
                cached_at(directory.path(), UpdateChannel::Stable, LINUX, now, fetch).unwrap(),
                Some(release())
            );
        }
        assert_eq!(calls.get(), 1);
        cached_at(
            directory.path(),
            UpdateChannel::Stable,
            LINUX,
            NOW + CACHE_TTL.as_secs(),
            fetch,
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn failures_are_cooled_down_without_stale_success() {
        let directory = tempdir().unwrap();
        cached_at(directory.path(), UpdateChannel::Stable, LINUX, NOW, || {
            Ok(release())
        })
        .unwrap();
        let expired = NOW + CACHE_TTL.as_secs();
        assert!(matches!(
            cached_at(
                directory.path(),
                UpdateChannel::Stable,
                LINUX,
                expired,
                || Err(VersionError::Status(UNAVAILABLE))
            ),
            Err(VersionError::Status(UNAVAILABLE))
        ));
        let calls = Cell::new(0);
        let fetch = || {
            calls.set(calls.get() + 1);
            Ok(release())
        };
        let cached = cached_at(
            directory.path(),
            UpdateChannel::Stable,
            LINUX,
            expired + FAILURE_COOLDOWN.as_secs() - 1,
            fetch,
        )
        .unwrap();
        assert_eq!(cached, None);
        assert_eq!(calls.get(), 0);
        cached_at(
            directory.path(),
            UpdateChannel::Stable,
            LINUX,
            expired + FAILURE_COOLDOWN.as_secs(),
            fetch,
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn cache_keys_separate_channels_and_targets() {
        let directory = tempdir().unwrap();
        let calls = Cell::new(0);
        for (channel, target) in [
            (UpdateChannel::Stable, LINUX),
            (UpdateChannel::Preview, LINUX),
            (UpdateChannel::Stable, WINDOWS),
        ] {
            cached_at(directory.path(), channel, target, NOW, || {
                calls.set(calls.get() + 1);
                Ok(release())
            })
            .unwrap();
        }
        assert_eq!(calls.get(), 3);
    }

    #[test_case(b"{".to_vec(); "malformed")]
    #[test_case(vec![b' '; MAX_CACHE_BYTES + 1]; "oversized")]
    fn corrupt_cache_is_refetched(bytes: Vec<u8>) {
        let directory = tempdir().unwrap();
        let path = directory.path().join(format!("stable-{LINUX}.json"));
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            cached_at(directory.path(), UpdateChannel::Stable, LINUX, NOW, || Ok(
                release()
            ))
            .unwrap(),
            Some(release())
        );
        let entry: CacheEntry = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(entry.usable(&UpdateChannel::Stable, LINUX, NOW));
    }

    #[test_case("checked_at", json!(NOW + 1); "clock_rollback")]
    #[test_case("checked_at", json!(0); "clock_forward")]
    #[test_case("schema", json!(CACHE_SCHEMA + 1); "unknown_schema")]
    #[test_case("schema", json!(CACHE_SCHEMA - 1); "obsolete_asset_validation")]
    #[test_case("channel", json!("preview"); "wrong_channel")]
    #[test_case("target", json!(WINDOWS); "wrong_target")]
    #[test_case("release", json!({"tag": TAG, "version": "9.9.9"}); "version_mismatch")]
    #[test_case("release", json!({"tag": "v2.0.0-rc.1", "version": "2.0.0-rc.1"}); "preview_in_stable_cache")]
    fn invalid_cache_entries_are_refetched(field: &str, value: Value) {
        let directory = tempdir().unwrap();
        let mut entry = serde_json::to_value(CacheEntry {
            schema: CACHE_SCHEMA,
            channel: UpdateChannel::Stable,
            target: LINUX.to_owned(),
            checked_at: NOW,
            release: Some(release()),
        })
        .unwrap();
        entry[field] = value;
        fs::write(
            directory.path().join(format!("stable-{LINUX}.json")),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
        let calls = Cell::new(0);
        cached_at(directory.path(), UpdateChannel::Stable, LINUX, NOW, || {
            calls.set(calls.get() + 1);
            Ok(release())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn concurrent_cache_check_does_not_fetch_or_block() {
        let directory = tempdir().unwrap();
        let lock_path = directory.path().join(format!("stable-{LINUX}.lock"));
        let lock = try_exclusive_state_lock(&lock_path, LOCK_MODE)
            .unwrap()
            .unwrap();
        let calls = Cell::new(0);
        let fetch = || {
            calls.set(calls.get() + 1);
            Ok(release())
        };
        assert_eq!(
            cached_at(directory.path(), UpdateChannel::Stable, LINUX, NOW, fetch).unwrap(),
            None
        );
        assert_eq!(calls.get(), 0);
        drop(lock);
        assert_eq!(
            cached_at(directory.path(), UpdateChannel::Stable, LINUX, NOW, fetch).unwrap(),
            Some(release())
        );
        assert_eq!(calls.get(), 1);
    }
}
