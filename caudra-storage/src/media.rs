//! Content-addressed media: the bytes of every stored image or PDF live once
//! under `<state>/media/sha256/<2 hex>/<62 hex>`, and payloads carry only a
//! [`StoredMedia`] reference. No table tracks references: a file is live
//! while some stored payload names its hash, and [`MediaStore::prune`] deletes
//! the rest once they are older than [`MEDIA_GRACE`].
//!
//! A put finishes before the payload naming its hash commits, and a put that
//! finds the file already there refreshes its mtime, so a prune racing a new
//! reference never deletes what that reference is about to name.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, ErrorKind, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::ImageReader;
use lru::LruCache;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{StateDir, StorageError, sync_parent_dir_durable};

pub const MEDIA_DIR: &str = "media";
/// How long an unreferenced file survives, covering puts whose payload has
/// not committed yet. Matches the grace unreferenced tool outputs get.
pub const MEDIA_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HASH_DIR: &str = "sha256";
const HASH_HEX_LEN: usize = 64;
const SHARD_HEX_LEN: usize = 2;
const TEMP_PREFIX: &str = ".put-";
const CACHE_ENTRIES: usize = 512;
/// Base64 the process keeps in memory, so a serde round trip or a repeated
/// request does not read the same file again.
const CACHE_BYTES: usize = 192 * 1024 * 1024;
#[cfg(unix)]
const MEDIA_DIRECTORY_MODE: u32 = 0o700;

static CACHE: LazyLock<Mutex<ByteBoundedCache>> =
    LazyLock::new(|| Mutex::new(ByteBoundedCache::new()));
static HASH_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""sha256":"([0-9a-f]{64})""#).expect("media reference pattern is valid")
});

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("media store I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("media is not valid base64: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
    #[error("media file {sha256} does not match its hash")]
    Corrupt { sha256: MediaHash },
    #[error("invalid media hash: expected {HASH_HEX_LEN} lowercase hex digits")]
    InvalidHash,
}

impl From<StorageError> for MediaError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Io(error) => Self::Io(error),
            other => Self::Io(io::Error::other(other.to_string())),
        }
    }
}

impl From<MediaError> for StorageError {
    fn from(error: MediaError) -> Self {
        match error {
            MediaError::Io(error) => Self::Io(error),
            other => Self::Io(io::Error::other(other.to_string())),
        }
    }
}

/// The SHA-256 of a file's decoded bytes, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MediaHash(String);

impl MediaHash {
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        Self(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MediaHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for MediaHash {
    type Err = MediaError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid = value.len() == HASH_HEX_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        valid
            .then(|| Self(value.to_owned()))
            .ok_or(MediaError::InvalidHash)
    }
}

impl<'de> Deserialize<'de> for MediaHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// What a payload keeps of a stored file: its hash, its decoded size, and,
/// for an image whose header parsed, its dimensions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMedia {
    pub sha256: MediaHash,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// How every persisted image is written: a reference into the media store,
/// or, for an image that never reached it, its base64 inline. Images saved
/// before the store carried only `media_type` and `data`, which still read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredImage {
    Base64 {
        media_type: String,
        data: Arc<str>,
    },
    Media {
        media_type: String,
        #[serde(flatten)]
        media: StoredMedia,
    },
}

impl StoredImage {
    pub fn media_type(&self) -> &str {
        match self {
            Self::Base64 { media_type, .. } | Self::Media { media_type, .. } => media_type,
        }
    }

    pub fn decoded_bytes(&self) -> u64 {
        match self {
            Self::Base64 { data, .. } => data.len().div_ceil(4) as u64 * 3,
            Self::Media { media, .. } => media.bytes,
        }
    }

    /// Moves inline bytes into `store`. An image the store cannot take stays
    /// inline, since its bytes would otherwise be lost.
    pub fn stored_in(self, store: &MediaStore) -> Self {
        let Self::Base64 { media_type, data } = self else {
            return self;
        };
        match store.put_base64(&data) {
            Ok(media) => Self::Media { media_type, media },
            Err(error) => {
                tracing::warn!(%media_type, %error, "image kept inline");
                Self::Base64 { media_type, data }
            }
        }
    }
}

impl<'de> Deserialize<'de> for StoredImage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Tagged {
            Base64 {
                media_type: String,
                data: Arc<str>,
            },
            Media {
                media_type: String,
                #[serde(flatten)]
                media: StoredMedia,
            },
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Tagged(Tagged),
            Legacy { media_type: String, data: Arc<str> },
        }
        Ok(match Shape::deserialize(deserializer)? {
            Shape::Tagged(Tagged::Base64 { media_type, data })
            | Shape::Legacy { media_type, data } => Self::Base64 { media_type, data },
            Shape::Tagged(Tagged::Media { media_type, media }) => Self::Media { media_type, media },
        })
    }
}

/// Every media hash `payload` names. Over-matching only keeps a file longer,
/// so this reads raw JSON text without parsing it.
pub fn referenced_media(payload: &str, into: &mut HashSet<MediaHash>) {
    if !payload.contains("\"sha256\"") {
        return;
    }
    into.extend(
        HASH_REFERENCE
            .captures_iter(payload)
            .map(|captures| MediaHash(captures[1].to_owned())),
    );
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MediaPruneReport {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaHealth {
    Intact,
    Missing,
    Corrupt,
}

/// What reading back every file the live payloads name found. Both lists
/// are sorted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MediaCheckReport {
    pub referenced: usize,
    pub missing: Vec<MediaHash>,
    pub corrupt: Vec<MediaHash>,
}

impl MediaCheckReport {
    pub fn is_intact(&self) -> bool {
        self.missing.is_empty() && self.corrupt.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct MediaStore {
    root: PathBuf,
}

impl MediaStore {
    pub fn new(state_dir: &StateDir) -> Self {
        Self {
            root: state_dir.path().join(MEDIA_DIR),
        }
    }

    pub fn path(&self, sha256: &MediaHash) -> PathBuf {
        let (shard, rest) = sha256.as_str().split_at(SHARD_HEX_LEN);
        self.root.join(HASH_DIR).join(shard).join(rest)
    }

    pub fn put_base64(&self, data: &str) -> Result<StoredMedia, MediaError> {
        let bytes = BASE64.decode(data)?;
        let stored = self.put(&bytes)?;
        cache_insert(&stored.sha256, Arc::from(data));
        Ok(stored)
    }

    /// Writes `bytes` durably, or refreshes the mtime of the file already
    /// holding them.
    pub fn put(&self, bytes: &[u8]) -> Result<StoredMedia, MediaError> {
        let sha256 = MediaHash::of(bytes);
        let path = self.path(&sha256);
        let (width, height) = dimensions(bytes).unzip();
        let stored = StoredMedia {
            sha256,
            bytes: bytes.len() as u64,
            width,
            height,
        };
        if refresh(&path, stored.bytes)? {
            return Ok(stored);
        }
        let shard = path
            .parent()
            .expect("a media path always has a shard directory");
        self.create_dir_durable(shard)?;
        let mut temp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(shard)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        sync_parent_dir_durable(&path)?;
        Ok(stored)
    }

    /// The file's bytes as base64, or `None` when the file is gone.
    pub fn base64(&self, sha256: &MediaHash) -> Result<Option<Arc<str>>, MediaError> {
        let path = self.path(sha256);
        if !path.try_exists()? {
            cache_remove(sha256);
            return Ok(None);
        }
        if let Some(data) = cache_get(sha256) {
            return Ok(Some(data));
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if MediaHash::of(&bytes) != *sha256 {
            return Err(MediaError::Corrupt {
                sha256: sha256.clone(),
            });
        }
        let data: Arc<str> = Arc::from(BASE64.encode(&bytes));
        cache_insert(sha256, Arc::clone(&data));
        Ok(Some(data))
    }

    /// Deletes every file `referenced` does not name, and every abandoned
    /// temp file, last modified before `cutoff`.
    pub fn prune(
        &self,
        referenced: &HashSet<MediaHash>,
        cutoff: SystemTime,
    ) -> Result<MediaPruneReport, MediaError> {
        self.collect_unreferenced(referenced, cutoff, true)
    }

    /// How many files the store holds and their size, temp files included.
    pub fn usage(&self) -> MediaPruneReport {
        let mut usage = MediaPruneReport::default();
        let Ok(shards) = fs::read_dir(self.root.join(HASH_DIR)) else {
            return usage;
        };
        for shard in shards.flatten() {
            let Ok(files) = fs::read_dir(shard.path()) else {
                continue;
            };
            for metadata in files.flatten().filter_map(|file| file.metadata().ok()) {
                if metadata.is_file() {
                    usage.files += 1;
                    usage.bytes += metadata.len();
                }
            }
        }
        usage
    }

    /// How many of `hashes` the store holds and their size.
    pub fn usage_of<'a>(
        &self,
        hashes: impl IntoIterator<Item = &'a MediaHash>,
    ) -> MediaPruneReport {
        let mut usage = MediaPruneReport::default();
        for hash in hashes {
            if let Ok(metadata) = fs::metadata(self.path(hash))
                && metadata.is_file()
            {
                usage.files += 1;
                usage.bytes += metadata.len();
            }
        }
        usage
    }

    /// Reads the file back and compares its bytes with its hash. The cache is
    /// bypassed, so a file damaged after it was cached is still caught.
    pub fn verify(&self, sha256: &MediaHash) -> io::Result<MediaHealth> {
        match fs::read(self.path(sha256)) {
            Ok(bytes) if MediaHash::of(&bytes) == *sha256 => Ok(MediaHealth::Intact),
            Ok(_) => Ok(MediaHealth::Corrupt),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(MediaHealth::Missing),
            Err(error) => Err(error),
        }
    }

    /// What [`Self::prune`] would delete, deleting nothing.
    pub fn unreferenced(
        &self,
        referenced: &HashSet<MediaHash>,
        cutoff: SystemTime,
    ) -> Result<MediaPruneReport, MediaError> {
        self.collect_unreferenced(referenced, cutoff, false)
    }

    fn collect_unreferenced(
        &self,
        referenced: &HashSet<MediaHash>,
        cutoff: SystemTime,
        delete: bool,
    ) -> Result<MediaPruneReport, MediaError> {
        let mut report = MediaPruneReport::default();
        let hashes = self.root.join(HASH_DIR);
        let shards = match fs::read_dir(&hashes) {
            Ok(shards) => shards,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(report),
            Err(error) => return Err(error.into()),
        };
        for shard in shards {
            let shard = shard?;
            let shard_name = shard.file_name();
            let Some(shard_name) = shard_name
                .to_str()
                .filter(|name| name.len() == SHARD_HEX_LEN)
            else {
                continue;
            };
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(shard.path())? {
                let file = file?;
                let name = file.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let temp = name.starts_with(TEMP_PREFIX);
                let hash = format!("{shard_name}{name}").parse::<MediaHash>().ok();
                let unreferenced = hash.as_ref().is_some_and(|hash| !referenced.contains(hash));
                if !(temp || unreferenced) {
                    continue;
                }
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.modified()? >= cutoff {
                    continue;
                }
                if delete {
                    match &hash {
                        Some(hash) => {
                            if !remove_unless_refreshed(&file.path(), name, cutoff)? {
                                continue;
                            }
                            cache_remove(hash);
                        }
                        None => match fs::remove_file(file.path()) {
                            Ok(()) => {}
                            Err(error) if error.kind() == ErrorKind::NotFound => continue,
                            Err(error) => return Err(error.into()),
                        },
                    }
                }
                report.files += 1;
                report.bytes += metadata.len();
            }
        }
        Ok(report)
    }

    fn create_dir_durable(&self, dir: &Path) -> Result<(), MediaError> {
        if dir.is_dir() {
            return Ok(());
        }
        let missing: Vec<&Path> = dir
            .ancestors()
            .take_while(|ancestor| !ancestor.is_dir())
            .collect();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(MEDIA_DIRECTORY_MODE);
        }
        builder.create(dir)?;
        for created in missing.iter().rev() {
            sync_parent_dir_durable(created)?;
        }
        Ok(())
    }
}

/// Refreshes the mtime of the file at `path` when it holds `len` bytes,
/// returning whether it does. The caller replaces a file of another length or
/// one it may not touch.
fn refresh(path: &Path, len: u64) -> Result<bool, MediaError> {
    let file = match OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() != len {
        return Ok(false);
    }
    touch(&file)?;
    // A prune that moved the file aside before the touch either sees the new
    // mtime and puts it back, or has already taken it from this path.
    Ok(path.try_exists()?)
}

/// Deletes the unreferenced file at `path` unless a put refreshed it since it
/// was last read: the file is moved aside first, so a put that touches it
/// afterwards writes a fresh copy instead. A file left aside by a crash is an
/// old temp file the next prune deletes.
fn remove_unless_refreshed(
    path: &Path,
    name: &str,
    cutoff: SystemTime,
) -> Result<bool, MediaError> {
    let aside = path.with_file_name(format!("{TEMP_PREFIX}{name}"));
    match fs::rename(path, &aside) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    if fs::metadata(&aside)?.modified()? >= cutoff {
        fs::rename(&aside, path)?;
        return Ok(false);
    }
    fs::remove_file(&aside)?;
    Ok(true)
}

fn touch(file: &File) -> io::Result<()> {
    file.set_modified(SystemTime::now())
}

fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

struct ByteBoundedCache {
    entries: LruCache<MediaHash, Arc<str>>,
    bytes: usize,
}

impl ByteBoundedCache {
    fn new() -> Self {
        Self {
            entries: LruCache::new(
                NonZeroUsize::new(CACHE_ENTRIES).expect("cache capacity is nonzero"),
            ),
            bytes: 0,
        }
    }
}

fn cache_get(sha256: &MediaHash) -> Option<Arc<str>> {
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    cache.entries.get(sha256).cloned()
}

fn cache_insert(sha256: &MediaHash, data: Arc<str>) {
    if data.len() > CACHE_BYTES {
        return;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    let added = data.len();
    if let Some((_, evicted)) = cache.entries.push(sha256.clone(), data) {
        cache.bytes -= evicted.len();
    }
    cache.bytes += added;
    while cache.bytes > CACHE_BYTES {
        let Some((_, evicted)) = cache.entries.pop_lru() else {
            break;
        };
        cache.bytes -= evicted.len();
    }
}

fn cache_remove(sha256: &MediaHash) {
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(evicted) = cache.entries.pop(sha256) {
        cache.bytes -= evicted.len();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    const TEXT_BYTES: &[u8] = b"not an image";
    const AN_HOUR: Duration = Duration::from_secs(60 * 60);

    fn store() -> (TempDir, MediaStore) {
        let temp = TempDir::new().unwrap();
        let store = MediaStore::new(&StateDir::from_path(temp.path().to_path_buf()));
        (temp, store)
    }

    fn age(path: &Path, by: Duration) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - by).unwrap();
    }

    #[test]
    fn put_stores_once_under_the_hash_of_the_decoded_bytes() {
        let (_temp, store) = store();
        let first = store.put_base64(PIXEL_PNG).unwrap();
        let second = store.put_base64(PIXEL_PNG).unwrap();
        let bytes = BASE64.decode(PIXEL_PNG).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.sha256, MediaHash::of(&bytes));
        assert_eq!(first.bytes, bytes.len() as u64);
        assert_eq!((first.width, first.height), (Some(1), Some(1)));
        assert_eq!(fs::read(store.path(&first.sha256)).unwrap(), bytes);
        assert_eq!(
            store.base64(&first.sha256).unwrap().as_deref(),
            Some(PIXEL_PNG)
        );
    }

    #[cfg(unix)]
    #[test]
    fn put_writes_owner_only_files_in_owner_only_directories() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();
        let path = store.path(&stored.sha256);
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), MEDIA_DIRECTORY_MODE);
        assert_eq!(mode(&store.root), MEDIA_DIRECTORY_MODE);
    }

    #[test]
    fn a_put_of_bytes_already_stored_refreshes_them_against_pruning() {
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();
        let path = store.path(&stored.sha256);
        age(&path, MEDIA_GRACE + AN_HOUR);

        store.put(TEXT_BYTES).unwrap();
        let report = store
            .prune(&HashSet::new(), SystemTime::now() - MEDIA_GRACE)
            .unwrap();

        assert_eq!(report, MediaPruneReport::default());
        assert!(path.exists());
    }

    #[test]
    fn usage_counts_each_stored_file_once() {
        let (_temp, store) = store();
        assert_eq!(store.usage(), MediaPruneReport::default());
        store.put(TEXT_BYTES).unwrap();
        store.put(TEXT_BYTES).unwrap();
        store.put_base64(PIXEL_PNG).unwrap();

        let usage = store.usage();

        assert_eq!(usage.files, 2);
        assert_eq!(
            usage.bytes,
            (TEXT_BYTES.len() + BASE64.decode(PIXEL_PNG).unwrap().len()) as u64
        );
    }

    #[test]
    fn a_put_replaces_a_stored_file_of_the_wrong_length() {
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();
        let path = store.path(&stored.sha256);
        fs::write(&path, &TEXT_BYTES[1..]).unwrap();

        store.put(TEXT_BYTES).unwrap();

        assert_eq!(fs::read(&path).unwrap(), TEXT_BYTES);
    }

    #[test_case(AN_HOUR, true; "a file refreshed after the scan read it is put back")]
    #[test_case(MEDIA_GRACE + AN_HOUR, false; "a file still old is deleted")]
    fn a_pruned_file_is_moved_aside_before_it_is_deleted(aged: Duration, kept: bool) {
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();
        let path = store.path(&stored.sha256);
        age(&path, aged);
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();

        let removed =
            remove_unless_refreshed(&path, &name, SystemTime::now() - MEDIA_GRACE).unwrap();

        assert_eq!(removed, !kept);
        assert_eq!(path.exists(), kept);
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            usize::from(kept)
        );
    }

    #[test_case(true, false; "referenced old file is kept")]
    #[test_case(false, true; "unreferenced old file is deleted")]
    fn prune_deletes_only_old_unreferenced_files(referenced: bool, deleted: bool) {
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();
        let path = store.path(&stored.sha256);
        age(&path, MEDIA_GRACE + AN_HOUR);
        let roots: HashSet<MediaHash> = referenced
            .then(|| stored.sha256.clone())
            .into_iter()
            .collect();

        let report = store
            .prune(&roots, SystemTime::now() - MEDIA_GRACE)
            .unwrap();

        assert_eq!(!path.exists(), deleted);
        assert_eq!(report.files, usize::from(deleted));
    }

    #[test]
    fn prune_keeps_a_recent_unreferenced_file_whose_payload_may_not_have_committed() {
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();

        store
            .prune(&HashSet::new(), SystemTime::now() - MEDIA_GRACE)
            .unwrap();

        assert!(store.path(&stored.sha256).exists());
    }

    #[test_case(MEDIA_GRACE + AN_HOUR, false; "abandoned temp file is deleted")]
    #[test_case(AN_HOUR, true; "temp file of a put in flight is kept")]
    fn prune_clears_only_abandoned_temp_files(aged: Duration, kept: bool) {
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();
        let shard = store.path(&stored.sha256).parent().unwrap().to_path_buf();
        let temp = shard.join(format!("{TEMP_PREFIX}crashed"));
        fs::write(&temp, TEXT_BYTES).unwrap();
        age(&temp, aged);

        store
            .prune(
                &HashSet::from([stored.sha256]),
                SystemTime::now() - MEDIA_GRACE,
            )
            .unwrap();

        assert_eq!(temp.exists(), kept);
    }

    #[test]
    fn a_missing_file_reads_as_none_even_after_it_was_cached() {
        let (_temp, store) = store();
        let stored = store.put_base64(PIXEL_PNG).unwrap();
        fs::remove_file(store.path(&stored.sha256)).unwrap();

        assert_eq!(store.base64(&stored.sha256).unwrap(), None);
    }

    #[test]
    fn a_file_that_no_longer_matches_its_hash_is_refused() {
        let (_temp, store) = store();
        let stored = store.put(TEXT_BYTES).unwrap();
        cache_remove(&stored.sha256);
        fs::write(store.path(&stored.sha256), b"tampered").unwrap();

        assert!(matches!(
            store.base64(&stored.sha256),
            Err(MediaError::Corrupt { .. })
        ));
    }

    #[test]
    fn references_are_read_from_raw_payload_text() {
        let stored = StoredMedia {
            sha256: MediaHash::of(TEXT_BYTES),
            bytes: TEXT_BYTES.len() as u64,
            width: None,
            height: None,
        };
        let payload = serde_json::to_string(&StoredImage::Media {
            media_type: "image/png".into(),
            media: stored.clone(),
        })
        .unwrap();
        let mut found = HashSet::new();

        referenced_media(&payload, &mut found);

        assert_eq!(found, HashSet::from([stored.sha256]));
    }

    #[test_case(r#"{"media_type":"image/png","data":"aGk="}"#; "legacy untagged")]
    #[test_case(r#"{"type":"base64","media_type":"image/png","data":"aGk="}"#; "tagged inline")]
    fn inline_images_of_either_shape_read(json: &str) {
        let image: StoredImage = serde_json::from_str(json).unwrap();

        assert_eq!(
            image,
            StoredImage::Base64 {
                media_type: "image/png".into(),
                data: Arc::from("aGk="),
            }
        );
    }

    #[test]
    fn an_image_moved_into_the_store_round_trips_as_a_reference() {
        let (_temp, store) = store();
        let image = StoredImage::Base64 {
            media_type: "image/png".into(),
            data: Arc::from(PIXEL_PNG),
        }
        .stored_in(&store);
        let json = serde_json::to_string(&image).unwrap();

        assert!(matches!(image, StoredImage::Media { .. }));
        assert!(!json.contains(PIXEL_PNG));
        assert_eq!(serde_json::from_str::<StoredImage>(&json).unwrap(), image);
    }
}
