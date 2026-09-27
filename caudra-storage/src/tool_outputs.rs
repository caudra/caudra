use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fmt;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use regex::Regex;
use regex_automata::Anchored;
use regex_automata::hybrid::LazyStateID;
use regex_automata::hybrid::dfa::{Cache as DfaCache, DFA};
use regex_automata::hybrid::regex::Regex as HybridRegex;
use regex_automata::nfa::thompson::{NFA, State};
use regex_automata::util::look::Look;
use regex_automata::util::primitives::StateID;
use regex_automata::util::start::Config as StartConfig;
#[cfg(unix)]
use rustix::fd::OwnedFd;
#[cfg(unix)]
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde::{Deserialize, Deserializer, Serialize};
#[cfg(not(unix))]
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::id::CaudraId;
use crate::words::random_task_id;
use crate::{StateDir, StorageError, lock_session_artifacts, sync_parent_dir};

pub(crate) const TOOL_OUTPUT_DIR: &str = "tool-output";
const OUTPUT_EXTENSION: &str = "txt";
const DEFAULT_MAX_STORED_BYTES: usize = 100 * 1024 * 1024;
pub const TOOL_OUTPUT_CONTROL_RESERVE_BYTES: usize = 256;
const MAX_READ_LINES: usize = 2_000;
const MAX_READ_BYTES: usize = 50 * 1024;
const MAX_DISPLAY_LINE_BYTES: usize = 2_000;
const GREP_FORMAT_RESERVE_BYTES: usize = 8 * 1024;
const GREP_ROW_BYTES: usize = MAX_READ_BYTES - GREP_FORMAT_RESERVE_BYTES;
const MAX_GREP_ROWS: usize = MAX_READ_LINES - 2;
const MAX_GREP_PATTERN_CHARS: usize = 512;
const MAX_GREP_MATCHES: usize = 200;
const MAX_GREP_CONTEXT: usize = 5;
const ORPHAN_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const ID_GENERATION_ATTEMPTS: usize = 8;
const MAX_OUTPUT_ID_BYTES: usize = 64;
const OUTPUT_ID_WORDS: usize = 3;
const SCAN_BUFFER_BYTES: usize = 64 * 1024;
#[cfg(unix)]
const TEMP_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const DIRECTORY_MODE: u32 = 0o700;
const OMITTED: &str = "[...]";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolOutputId(String);

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("invalid tool output ID: expected three lowercase words or a canonical legacy ID")]
pub struct ToolOutputIdParseError;

impl ToolOutputId {
    fn generate() -> Result<Self, ToolOutputError> {
        let phrase = random_task_id().map_err(std::io::Error::other)?;
        phrase
            .parse()
            .map_err(|error| std::io::Error::other(error).into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolOutputId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ToolOutputId {
    type Err = ToolOutputIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > MAX_OUTPUT_ID_BYTES {
            return Err(ToolOutputIdParseError);
        }
        let mut words = value.split('-');
        let readable = (0..OUTPUT_ID_WORDS).all(|_| {
            words.next().is_some_and(|word| {
                !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase())
            })
        }) && words.next().is_none();
        if readable
            || value
                .parse::<CaudraId>()
                .is_ok_and(|id| id.to_string() == value)
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(ToolOutputIdParseError)
        }
    }
}

impl<'de> Deserialize<'de> for ToolOutputId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputRef {
    pub id: ToolOutputId,
    pub byte_count: usize,
    pub line_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputReadResult {
    pub text: String,
    pub offset: usize,
    pub byte_offset: usize,
    pub returned_lines: usize,
    pub next_offset: Option<usize>,
    pub next_byte_offset: usize,
    pub total_lines: usize,
    pub total_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputGrepRow {
    pub line_number: usize,
    pub text: String,
    pub is_match: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputGrepResult {
    pub rows: Vec<ToolOutputGrepRow>,
    pub next_offset: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutputPreview {
    pub head: String,
    pub tail: String,
}

impl ToolOutputPreview {
    pub fn into_text(self) -> String {
        if self.head.is_empty() {
            return self.tail;
        }
        if self.tail.is_empty() {
            return self.head;
        }
        self.head + "\n" + &self.tail
    }
}

#[derive(Debug, Error)]
pub enum ToolOutputError {
    #[error("tool output is {byte_count} bytes, exceeding the {max_bytes}-byte limit")]
    TooLarge { byte_count: usize, max_bytes: usize },
    #[error("tool output {output_id} does not exist for session {session_id}")]
    NotFound {
        session_id: CaudraId,
        output_id: ToolOutputId,
    },
    #[error("tool output {output_id} for session {session_id} is not valid UTF-8")]
    InvalidUtf8 {
        session_id: CaudraId,
        output_id: ToolOutputId,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("offset must be at least 1")]
    InvalidOffset,
    #[error("byte offset {byte_offset} is invalid for line {line_number}")]
    InvalidByteOffset {
        line_number: usize,
        byte_offset: usize,
    },
    #[error("limit must be at least 1")]
    InvalidLimit,
    #[error("grep pattern is {char_count} characters, exceeding the {max_chars}-character limit")]
    PatternTooLong { char_count: usize, max_chars: usize },
    #[error("invalid grep pattern: {0}")]
    InvalidPattern(#[from] regex::Error),
    #[error("could not generate a unique tool output ID")]
    IdCollision,
    #[error("tool output sink cannot be finished after an I/O write failure")]
    SinkWriteFailed,
    #[error("tool output changed while it was being copied")]
    ChangedDuringCopy,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[derive(Clone, Debug)]
pub struct ToolOutputStore {
    state_dir: StateDir,
    max_bytes: usize,
    orphan_grace: Duration,
}

#[cfg(unix)]
struct OutputDirectory {
    fd: OwnedFd,
    path: PathBuf,
    created: bool,
}

#[cfg(not(unix))]
struct OutputDirectory {
    path: PathBuf,
    created: bool,
}

impl fmt::Debug for OutputDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputDirectory")
            .field("path", &self.path)
            .field("created", &self.created)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
struct StagedOutput {
    file: File,
    dir: OwnedFd,
    name: Option<String>,
    path: PathBuf,
}

#[cfg(not(unix))]
struct StagedOutput {
    file: Option<NamedTempFile>,
    published: Option<File>,
    path: PathBuf,
}

impl fmt::Debug for StagedOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedOutput")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl StagedOutput {
    fn file(&self) -> std::io::Result<&File> {
        #[cfg(unix)]
        return Ok(&self.file);
        #[cfg(not(unix))]
        return self
            .file
            .as_ref()
            .map(NamedTempFile::as_file)
            .or(self.published.as_ref())
            .ok_or_else(|| std::io::Error::other("staged output was already published"));
    }

    fn file_mut(&mut self) -> std::io::Result<&mut File> {
        #[cfg(unix)]
        return Ok(&mut self.file);
        #[cfg(not(unix))]
        return self
            .file
            .as_mut()
            .map(NamedTempFile::as_file_mut)
            .ok_or_else(|| std::io::Error::other("staged output was already published"));
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for StagedOutput {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            let _ = rustix::fs::unlinkat(&self.dir, name, AtFlags::empty());
        }
    }
}

#[derive(Debug)]
pub struct ToolOutputSink {
    state_dir: StateDir,
    file: Option<StagedOutput>,
    directory: OutputDirectory,
    id: ToolOutputId,
    byte_count: usize,
    newline_count: usize,
    ends_with_newline: bool,
    max_bytes: usize,
    write_failed: bool,
}

impl ToolOutputSink {
    pub fn append(&mut self, text: &str) -> Result<(), ToolOutputError> {
        self.append_with_reserve(text, 0)
    }

    pub fn append_with_reserve(
        &mut self,
        text: &str,
        reserve_bytes: usize,
    ) -> Result<(), ToolOutputError> {
        if self.write_failed {
            return Err(ToolOutputError::SinkWriteFailed);
        }
        let byte_count = self.byte_count.saturating_add(text.len());
        let max_bytes = self.max_bytes.saturating_sub(reserve_bytes);
        if byte_count > max_bytes {
            return Err(ToolOutputError::TooLarge {
                byte_count,
                max_bytes,
            });
        }

        let Some(file) = self.file.as_mut() else {
            return Err(ToolOutputError::SinkWriteFailed);
        };
        if let Err(error) = file
            .file_mut()
            .and_then(|file| file.write_all(text.as_bytes()))
        {
            self.write_failed = true;
            return Err(error.into());
        }
        self.byte_count = byte_count;
        self.newline_count += text.bytes().filter(|byte| *byte == b'\n').count();
        if let Some(last) = text.as_bytes().last() {
            self.ends_with_newline = *last == b'\n';
        }
        Ok(())
    }

    pub fn finish(self) -> Result<ToolOutputRef, ToolOutputError> {
        self.finish_with(ToolOutputId::generate)
    }

    fn finish_with(
        mut self,
        mut generate: impl FnMut() -> Result<ToolOutputId, ToolOutputError>,
    ) -> Result<ToolOutputRef, ToolOutputError> {
        if self.write_failed {
            return Err(ToolOutputError::SinkWriteFailed);
        }
        let Some(mut file) = self.file.take() else {
            return Err(ToolOutputError::SinkWriteFailed);
        };
        file.file_mut()?.flush()?;
        file.file()?.sync_all()?;
        // Cleanup and recreation use this same cross-process lock, so an old
        // generation cannot delete an output while publication is in flight.
        let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
        for attempt in 0..ID_GENERATION_ATTEMPTS {
            if attempt > 0 {
                self.id = generate()?;
            }
            match self.directory.publish(&mut file, &output_name(&self.id)) {
                Ok(()) => {
                    self.directory.sync()?;
                    return Ok(ToolOutputRef {
                        id: self.id.clone(),
                        byte_count: self.byte_count,
                        line_count: logical_line_count(
                            self.byte_count,
                            self.newline_count,
                            self.ends_with_newline,
                        ),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(ToolOutputError::IdCollision)
    }

    pub fn discard(mut self) -> Result<(), ToolOutputError> {
        self.file.take();
        self.directory.remove_if_created();
        Ok(())
    }
}

impl Drop for ToolOutputSink {
    fn drop(&mut self) {
        self.file.take();
        self.directory.remove_if_created();
    }
}

impl OutputDirectory {
    #[cfg(unix)]
    fn open(state_dir: &StateDir, session_id: CaudraId, create: bool) -> std::io::Result<Self> {
        if create {
            fs::create_dir_all(state_dir.path())?;
        }
        let state = rustix::fs::open(
            state_dir.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let root_created = create_directory_at(&state, TOOL_OUTPUT_DIR, create)?;
        let root = rustix::fs::openat(
            &state,
            TOOL_OUTPUT_DIR,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        if root_created {
            rustix::fs::fsync(&state).map_err(std::io::Error::from)?;
        }

        let session_name = session_id.to_string();
        let created = create_directory_at(&root, &session_name, create)?;
        let fd = rustix::fs::openat(
            &root,
            &session_name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        if created {
            rustix::fs::fsync(&root).map_err(std::io::Error::from)?;
        }
        Ok(Self {
            fd,
            path: state_dir.path().join(TOOL_OUTPUT_DIR).join(session_name),
            created,
        })
    }

    #[cfg(not(unix))]
    fn open(state_dir: &StateDir, session_id: CaudraId, create: bool) -> std::io::Result<Self> {
        let root = state_dir.path().join(TOOL_OUTPUT_DIR);
        if create {
            create_checked_directory(state_dir.path())?;
            create_checked_directory(&root)?;
        } else {
            validate_directory(&root)?;
        }
        let path = root.join(session_id.to_string());
        let created = if create && !path.try_exists()? {
            fs::create_dir(&path)?;
            sync_parent_dir(&path);
            true
        } else {
            false
        };
        validate_directory(&path)?;
        Ok(Self { path, created })
    }

    #[cfg(unix)]
    fn contains(&self, name: &str) -> std::io::Result<bool> {
        match rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(not(unix))]
    fn contains(&self, name: &str) -> std::io::Result<bool> {
        match fs::symlink_metadata(self.path.join(name)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    #[cfg(unix)]
    fn open_regular(&self, name: &str) -> std::io::Result<File> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let stat = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
        if !FileType::from_raw_mode(stat.st_mode).is_file() {
            return Err(not_regular_file());
        }
        Ok(File::from(fd))
    }

    #[cfg(not(unix))]
    fn open_regular(&self, name: &str) -> std::io::Result<File> {
        let path = self.path.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(not_regular_file());
        }
        let options = OpenOptions::new();
        #[cfg(windows)]
        let mut options = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            let mut options = options;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            options
        };
        #[cfg(not(windows))]
        let mut options = options;
        let file = options.read(true).open(path)?;
        if !file.metadata()?.is_file() {
            return Err(not_regular_file());
        }
        Ok(file)
    }

    fn create_stage(&self) -> std::io::Result<StagedOutput> {
        for _ in 0..ID_GENERATION_ATTEMPTS {
            #[cfg(unix)]
            {
                let name = format!(".{}.tmp", CaudraId::generate());
                let path = self.path.join(&name);
                match rustix::fs::openat(
                    &self.fd,
                    &name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(TEMP_FILE_MODE),
                ) {
                    Ok(fd) => {
                        return Ok(StagedOutput {
                            file: File::from(fd),
                            dir: rustix::io::dup(&self.fd).map_err(std::io::Error::from)?,
                            name: Some(name),
                            path,
                        });
                    }
                    Err(rustix::io::Errno::EXIST) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(not(unix))]
            match NamedTempFile::new_in(&self.path) {
                Ok(file) => {
                    return Ok(StagedOutput {
                        path: file.path().to_path_buf(),
                        file: Some(file),
                        published: None,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "could not create a unique output staging file",
        ))
    }

    #[cfg(unix)]
    fn publish(&self, staged: &mut StagedOutput, name: &str) -> std::io::Result<()> {
        let Some(temp_name) = staged.name.as_deref() else {
            return Err(std::io::Error::other("staged output was already published"));
        };
        rustix::fs::linkat(&staged.dir, temp_name, &self.fd, name, AtFlags::empty())
            .map_err(std::io::Error::from)?;
        if rustix::fs::unlinkat(&staged.dir, temp_name, AtFlags::empty()).is_ok() {
            staged.name = None;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn publish(&self, staged: &mut StagedOutput, name: &str) -> std::io::Result<()> {
        let target = self.path.join(name);
        let mut file = staged
            .file
            .take()
            .ok_or_else(|| std::io::Error::other("staged output was already published"))?;
        #[cfg(windows)]
        {
            const RENAME_ATTEMPTS: usize = 20;
            let mut previous = 0u64;
            let mut delay = 1u64;
            for _ in 0..RENAME_ATTEMPTS {
                match file.persist_noclobber(&target) {
                    Ok(file) => {
                        staged.published = Some(file);
                        return Ok(());
                    }
                    Err(error) if error.error.kind() == ErrorKind::PermissionDenied => {
                        file = error.file;
                        std::thread::sleep(Duration::from_millis(delay));
                        (previous, delay) = (delay, previous.saturating_add(delay));
                    }
                    Err(error) => {
                        staged.file = Some(error.file);
                        return Err(error.error);
                    }
                }
            }
        }
        match file.persist_noclobber(target) {
            Ok(file) => {
                staged.published = Some(file);
                Ok(())
            }
            Err(error) => {
                staged.file = Some(error.file);
                Err(error.error)
            }
        }
    }

    #[cfg(unix)]
    fn remove_created_file(&self, name: &str, source: &File) {
        let Ok(source_stat) = rustix::fs::fstat(source) else {
            return;
        };
        let Ok(target_stat) = rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
            return;
        };
        if source_stat.st_dev == target_stat.st_dev && source_stat.st_ino == target_stat.st_ino {
            let _ = rustix::fs::unlinkat(&self.fd, name, AtFlags::empty());
        }
    }

    #[cfg(not(unix))]
    fn remove_created_file(&self, name: &str, _source: &File) {
        let _ = fs::remove_file(self.path.join(name));
    }

    fn sync(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        rustix::fs::fsync(&self.fd).map_err(std::io::Error::from)?;
        #[cfg(not(unix))]
        crate::sync_parent_dir_durable(&self.path.join("entry")).map_err(std::io::Error::from)?;
        Ok(())
    }

    fn remove_if_created(&mut self) {
        if !self.created {
            return;
        }
        if fs::remove_dir(&self.path).is_ok() {
            sync_parent_dir(&self.path);
            self.created = false;
        }
    }
}

#[cfg(unix)]
fn create_directory_at(dir: &OwnedFd, name: &str, create: bool) -> std::io::Result<bool> {
    if !create {
        return Ok(false);
    }
    match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(DIRECTORY_MODE)) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::EXIST) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn create_checked_directory(path: &Path) -> std::io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => sync_parent_dir(path),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_directory(path)
}

#[cfg(not(unix))]
fn validate_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "tool output session path is not a real directory",
        ));
    }
    Ok(())
}

fn not_regular_file() -> std::io::Error {
    std::io::Error::new(
        ErrorKind::InvalidInput,
        "tool output artifact is not a regular file",
    )
}

impl ToolOutputStore {
    pub fn state_dir(&self) -> &StateDir {
        &self.state_dir
    }

    pub fn new(state_dir: StateDir) -> Self {
        Self::with_max_bytes(state_dir, DEFAULT_MAX_STORED_BYTES)
    }

    pub fn with_max_bytes(state_dir: StateDir, max_bytes: usize) -> Self {
        Self {
            state_dir,
            max_bytes,
            orphan_grace: ORPHAN_GRACE,
        }
    }

    pub fn put(&self, session_id: CaudraId, text: &str) -> Result<ToolOutputRef, ToolOutputError> {
        self.ensure_size(text.len())?;
        let mut sink = self.begin(session_id)?;
        sink.append(text)?;
        sink.finish()
    }

    pub fn begin(&self, session_id: CaudraId) -> Result<ToolOutputSink, ToolOutputError> {
        self.begin_with(session_id, ToolOutputId::generate)
    }

    fn begin_with(
        &self,
        session_id: CaudraId,
        mut generate: impl FnMut() -> Result<ToolOutputId, ToolOutputError>,
    ) -> Result<ToolOutputSink, ToolOutputError> {
        let mut directory = OutputDirectory::open(&self.state_dir, session_id, true)?;

        for _ in 0..ID_GENERATION_ATTEMPTS {
            let id = generate()?;
            let final_name = output_name(&id);
            if directory.contains(&final_name)? {
                continue;
            }
            let file = match directory.create_stage() {
                Ok(file) => file,
                Err(error) => {
                    directory.remove_if_created();
                    return Err(error.into());
                }
            };
            return Ok(ToolOutputSink {
                state_dir: self.state_dir.clone(),
                file: Some(file),
                directory,
                id,
                byte_count: 0,
                newline_count: 0,
                ends_with_newline: false,
                max_bytes: self.max_bytes,
                write_failed: false,
            });
        }

        directory.remove_if_created();
        Err(ToolOutputError::IdCollision)
    }

    pub fn read(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        offset: usize,
        limit: usize,
    ) -> Result<ToolOutputReadResult, ToolOutputError> {
        self.read_at(session_id, id, offset, limit, 0)
    }

    pub fn read_at(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        offset: usize,
        limit: usize,
        byte_offset: usize,
    ) -> Result<ToolOutputReadResult, ToolOutputError> {
        validate_page(offset, limit)?;
        let mut file = self.open_output(session_id, &id)?;
        let mut scan = ReadScan::new(offset - 1, limit.min(MAX_READ_LINES), byte_offset);
        let summary = scan_utf8_lines(&mut file, self.max_bytes, session_id, id, &mut scan)?;
        scan.finish()?;
        Ok(ToolOutputReadResult {
            text: scan.page,
            offset,
            byte_offset,
            returned_lines: scan.returned_lines,
            next_offset: scan.next_offset,
            next_byte_offset: scan.next_byte_offset,
            total_lines: summary.total_lines,
            total_bytes: summary.total_bytes,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn grep(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        pattern: &str,
        offset: usize,
        limit: usize,
        context_before: usize,
        context_after: usize,
    ) -> Result<ToolOutputGrepResult, ToolOutputError> {
        validate_page(offset, limit)?;
        let char_count = pattern.chars().count();
        if char_count > MAX_GREP_PATTERN_CHARS {
            return Err(ToolOutputError::PatternTooLong {
                char_count,
                max_chars: MAX_GREP_PATTERN_CHARS,
            });
        }

        Regex::new(pattern)?;
        let nfa = NFA::new(pattern).map_err(std::io::Error::other)?;
        let hybrid = if nfa.has_empty() || nfa.look_set_any().contains_word_unicode() {
            None
        } else {
            Some(HybridRegex::new(pattern).map_err(std::io::Error::other)?)
        };
        let limit = limit.min(MAX_GREP_MATCHES);
        let context_before = context_before.min(MAX_GREP_CONTEXT);
        let context_after = context_after.min(MAX_GREP_CONTEXT);
        let mut file = self.open_output(session_id, &id)?;
        let mut scan = GrepScan::new(
            &nfa,
            hybrid.as_ref(),
            offset - 1,
            limit,
            context_before,
            context_after,
        );
        scan_utf8_lines(&mut file, self.max_bytes, session_id, id, &mut scan)?;
        if let Some(regex) = &hybrid {
            resolve_match_starts(&mut file, regex, &mut scan.rows, &mut scan.matches)?;
        }
        let (mut rows, budget_omitted) = read_grep_rows(&mut file, &scan.rows, &scan.matches)?;
        let first_omitted = match (budget_omitted, scan.next_match) {
            (Some(budget), Some(count)) => Some(if budget.index <= count.index {
                budget
            } else {
                count
            }),
            (Some(budget), None) => Some(budget),
            (None, count) => count,
        };
        if let Some(omitted) = &first_omitted {
            rows.retain(|row| row.line_number <= omitted.index);
        }
        let next_offset = first_omitted.map(|line| line.index + 1);

        Ok(ToolOutputGrepResult { rows, next_offset })
    }

    pub fn preview(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<ToolOutputPreview, ToolOutputError> {
        if max_lines == 0 || max_bytes == 0 {
            return Ok(ToolOutputPreview {
                head: String::new(),
                tail: String::new(),
            });
        }

        let mut file = self.open_output(session_id, &id)?;
        let total_bytes = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
        self.ensure_size(total_bytes)?;
        let payload_bytes = max_bytes.saturating_sub(1);
        let head_bytes = payload_bytes.div_ceil(2);
        let tail_bytes = payload_bytes / 2;
        let head = read_head(&mut file, max_lines.div_ceil(2), head_bytes)?;
        let tail = read_tail(&mut file, max_lines / 2, tail_bytes)?;
        Ok(ToolOutputPreview { head, tail })
    }

    pub fn delete_session(&self, session_id: CaudraId) -> Result<(), ToolOutputError> {
        let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
        delete_session_outputs(&self.state_dir, session_id).map_err(Into::into)
    }

    pub fn copy_session_outputs(
        &self,
        source: CaudraId,
        target: CaudraId,
        references: &[ToolOutputRef],
    ) -> Result<(), ToolOutputError> {
        if references.is_empty() {
            return Ok(());
        }
        let mut seen = HashSet::new();
        let mut sources = Vec::new();
        for reference in references {
            if !seen.insert(&reference.id) {
                continue;
            }
            let file = self.open_output(source, &reference.id)?;
            let byte_count = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
            self.ensure_size(byte_count)?;
            sources.push((reference.id.clone(), file, byte_count));
        }
        if source == target {
            return Ok(());
        }

        let mut directory = OutputDirectory::open(&self.state_dir, target, true)?;
        let result = self.copy_into_directory(&mut directory, &mut sources);
        if result.is_err() {
            directory.remove_if_created();
        }
        result
    }

    fn copy_into_directory(
        &self,
        directory: &mut OutputDirectory,
        sources: &mut [(ToolOutputId, File, usize)],
    ) -> Result<(), ToolOutputError> {
        let mut staged = Vec::with_capacity(sources.len());
        for (id, source, initial_len) in sources {
            let mut output = directory.create_stage()?;
            let copied = std::io::copy(
                &mut source.take(self.max_bytes.saturating_add(1) as u64),
                output.file_mut()?,
            )?;
            let copied = usize::try_from(copied).unwrap_or(usize::MAX);
            self.ensure_size(copied)?;
            let final_len = usize::try_from(source.metadata()?.len()).unwrap_or(usize::MAX);
            if copied != *initial_len || final_len != *initial_len {
                return Err(ToolOutputError::ChangedDuringCopy);
            }
            output.file_mut()?.flush()?;
            output.file()?.sync_all()?;
            staged.push((id.clone(), output));
        }

        let mut published: Vec<(String, usize)> = Vec::with_capacity(staged.len());
        let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
        for index in 0..staged.len() {
            let name = output_name(&staged[index].0);
            if let Err(error) = directory.publish(&mut staged[index].1, &name) {
                for (published_name, published_index) in &published {
                    if let Ok(file) = staged[*published_index].1.file() {
                        directory.remove_created_file(published_name, file);
                    }
                }
                let _ = directory.sync();
                return Err(error.into());
            }
            published.push((name, index));
        }
        directory.sync()?;
        Ok(())
    }

    pub fn cleanup_orphans(&self, live_session_ids: &[CaudraId]) -> Result<usize, ToolOutputError> {
        self.visit_orphans(live_session_ids, false)
    }

    /// The entries `cleanup_orphans` would remove right now.
    pub fn count_orphans(&self, live_session_ids: &[CaudraId]) -> Result<u64, ToolOutputError> {
        self.visit_orphans(live_session_ids, true)
            .map(|count| u64::try_from(count).unwrap_or(u64::MAX))
    }

    fn visit_orphans(
        &self,
        live_session_ids: &[CaudraId],
        dry_run: bool,
    ) -> Result<usize, ToolOutputError> {
        let root = self.root();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let live: HashSet<CaudraId> = live_session_ids.iter().copied().collect();
        let now = SystemTime::now();
        let mut removed = 0;

        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let session_id = canonical_session_id(&entry.file_name());
            if file_type.is_dir()
                && let Some(session_id) = session_id
                && live.contains(&session_id)
            {
                let referenced = self.referenced_outputs(session_id);
                let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
                removed +=
                    self.cleanup_live_session(&entry.path(), now, referenced.as_ref(), dry_run)?;
            } else {
                let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
                if is_stale(&entry.path(), now, self.orphan_grace)?
                    && (dry_run || remove_artifact(&entry.path(), file_type.is_dir())?)
                {
                    removed += 1;
                }
            }
        }

        Ok(removed)
    }

    fn cleanup_live_session(
        &self,
        session_dir: &Path,
        now: SystemTime,
        referenced: Option<&HashSet<ToolOutputId>>,
        dry_run: bool,
    ) -> Result<usize, ToolOutputError> {
        let mut removed = 0;
        for entry in fs::read_dir(session_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_file()
                && let Some(output_id) = owned_output_id(&entry.file_name())
                && referenced.is_none_or(|ids| ids.contains(&output_id))
            {
                continue;
            }
            if is_stale(&entry.path(), now, self.orphan_grace)?
                && (dry_run || remove_artifact(&entry.path(), file_type.is_dir())?)
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn referenced_outputs(&self, session_id: CaudraId) -> Option<HashSet<ToolOutputId>> {
        let sessions_dir = self.state_dir.path().join(crate::sessions::SESSIONS_DIR);
        let mut referenced = HashSet::new();
        // `None` is fail-closed: callers retain every managed output whenever
        // the canonical reference snapshot cannot be read or parsed.
        match crate::sessions::SessionDatabase::open(&self.state_dir) {
            Ok(database) => {
                let mut valid = true;
                match database.visit_payload_json(session_id, |payload| {
                    let Ok(value) = serde_json::from_str(payload) else {
                        valid = false;
                        return;
                    };
                    collect_output_ids(&value, &mut referenced);
                }) {
                    Ok(()) if valid => {}
                    Ok(()) => return None,
                    Err(_) => return None,
                }
            }
            Err(_) => return None,
        }
        let archive_dir = sessions_dir
            .join(crate::sessions::ARCHIVE_DIR)
            .join(session_id.to_string());
        match fs::read_dir(archive_dir) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry.ok()?.path();
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "jsonl")
                    {
                        collect_references_from_session_file(&path, &mut referenced)?;
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        Some(referenced)
    }

    fn ensure_size(&self, byte_count: usize) -> Result<(), ToolOutputError> {
        if byte_count > self.max_bytes {
            return Err(ToolOutputError::TooLarge {
                byte_count,
                max_bytes: self.max_bytes,
            });
        }
        Ok(())
    }

    pub fn load_text(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
    ) -> Result<String, ToolOutputError> {
        let file = self.open_output(session_id, &id)?;
        let mut bytes = Vec::new();
        file.take(self.max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        self.ensure_size(bytes.len())?;
        String::from_utf8(bytes).map_err(|error| ToolOutputError::InvalidUtf8 {
            session_id,
            output_id: id,
            source: error.utf8_error(),
        })
    }

    fn open_output(
        &self,
        session_id: CaudraId,
        id: &ToolOutputId,
    ) -> Result<File, ToolOutputError> {
        let directory =
            OutputDirectory::open(&self.state_dir, session_id, false).map_err(|error| {
                if error.kind() == ErrorKind::NotFound {
                    ToolOutputError::NotFound {
                        session_id,
                        output_id: id.clone(),
                    }
                } else {
                    StorageError::Io(error).into()
                }
            })?;
        directory.open_regular(&output_name(id)).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                ToolOutputError::NotFound {
                    session_id,
                    output_id: id.clone(),
                }
            } else {
                StorageError::Io(error).into()
            }
        })
    }

    fn root(&self) -> PathBuf {
        self.state_dir.path().join(TOOL_OUTPUT_DIR)
    }

    #[cfg(test)]
    fn session_dir(&self, session_id: CaudraId) -> PathBuf {
        self.root().join(session_id.to_string())
    }

    #[cfg(test)]
    fn output_path(&self, session_id: CaudraId, id: ToolOutputId) -> PathBuf {
        self.session_dir(session_id).join(output_name(&id))
    }
}

fn output_name(id: &ToolOutputId) -> String {
    format!("{id}.{OUTPUT_EXTENSION}")
}

pub(crate) fn delete_session_outputs(
    state_dir: &StateDir,
    session_id: CaudraId,
) -> std::io::Result<()> {
    let root = state_dir.path().join(TOOL_OUTPUT_DIR);
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "tool output root is not a real directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let path = root.join(session_id.to_string());
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "tool output session path is not a real directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            crate::sync_parent_dir_io(&path)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        let directory = OutputDirectory::open(state_dir, session_id, false)?;
        drop(directory);
    }
    match fs::remove_dir_all(&path) {
        Ok(()) => {
            crate::sync_parent_dir_io(&path)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_page(offset: usize, limit: usize) -> Result<(), ToolOutputError> {
    if offset == 0 {
        return Err(ToolOutputError::InvalidOffset);
    }
    if limit == 0 {
        return Err(ToolOutputError::InvalidLimit);
    }
    Ok(())
}

pub fn line_count(text: &str) -> usize {
    logical_line_count(
        text.len(),
        text.bytes().filter(|byte| *byte == b'\n').count(),
        text.ends_with('\n'),
    )
}

fn logical_line_count(byte_count: usize, newline_count: usize, ends_with_newline: bool) -> usize {
    newline_count + usize::from(byte_count > 0 && !ends_with_newline)
}

#[derive(Clone, Debug)]
struct MatchingLine {
    index: usize,
    matched: Range<usize>,
}

#[derive(Clone, Copy, Debug)]
struct LineMeta {
    index: usize,
    start: usize,
    len: usize,
}

#[derive(Clone, Debug)]
struct PendingGrepRow {
    line: LineMeta,
    matched: Option<Range<usize>>,
}

#[derive(Clone, Copy, Debug)]
struct ScanSummary {
    total_lines: usize,
    total_bytes: usize,
}

trait LineSink {
    fn write(&mut self, text: &str, line_offset: usize);
    fn finish_line(&mut self, line: LineMeta, trailing_cr: bool) -> Result<(), ToolOutputError>;
}

fn scan_utf8_lines(
    file: &mut File,
    max_bytes: usize,
    session_id: CaudraId,
    output_id: ToolOutputId,
    sink: &mut impl LineSink,
) -> Result<ScanSummary, ToolOutputError> {
    file.seek(SeekFrom::Start(0))?;
    let mut read_buffer = vec![0; SCAN_BUFFER_BYTES.min(max_bytes.saturating_add(1)).max(1)];
    let mut decoded = Vec::with_capacity(read_buffer.len().saturating_add(3));
    let mut carry = Vec::with_capacity(3);
    let mut total_bytes = 0;
    let mut total_lines = 0;
    let mut line_start = 0;
    let mut line_len = 0;
    let mut line_ends_with_cr = false;

    loop {
        let remaining = max_bytes.saturating_sub(total_bytes).saturating_add(1);
        let read_len = read_buffer.len().min(remaining);
        let count = file.read(&mut read_buffer[..read_len])?;
        if count == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(count);
        if total_bytes > max_bytes {
            return Err(ToolOutputError::TooLarge {
                byte_count: total_bytes,
                max_bytes,
            });
        }

        decoded.clear();
        decoded.extend_from_slice(&carry);
        decoded.extend_from_slice(&read_buffer[..count]);
        carry.clear();
        let valid_len = match std::str::from_utf8(&decoded) {
            Ok(_) => decoded.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => {
                return Err(ToolOutputError::InvalidUtf8 {
                    session_id,
                    output_id,
                    source: error,
                });
            }
        };
        let valid_text = std::str::from_utf8(&decoded[..valid_len]).map_err(|source| {
            ToolOutputError::InvalidUtf8 {
                session_id,
                output_id: output_id.clone(),
                source,
            }
        })?;
        process_valid_text(
            valid_text,
            sink,
            &mut total_lines,
            &mut line_start,
            &mut line_len,
            &mut line_ends_with_cr,
        )?;
        carry.extend_from_slice(&decoded[valid_len..]);
    }

    if !carry.is_empty() {
        match std::str::from_utf8(&carry) {
            Ok(text) => process_valid_text(
                text,
                sink,
                &mut total_lines,
                &mut line_start,
                &mut line_len,
                &mut line_ends_with_cr,
            )?,
            Err(source) => {
                return Err(ToolOutputError::InvalidUtf8 {
                    session_id,
                    output_id,
                    source,
                });
            }
        }
    }
    if line_len > 0 {
        let logical_len = line_len.saturating_sub(usize::from(line_ends_with_cr));
        sink.finish_line(
            LineMeta {
                index: total_lines,
                start: line_start,
                len: logical_len,
            },
            line_ends_with_cr,
        )?;
        total_lines += 1;
    }
    Ok(ScanSummary {
        total_lines,
        total_bytes,
    })
}

fn process_valid_text(
    mut text: &str,
    sink: &mut impl LineSink,
    total_lines: &mut usize,
    line_start: &mut usize,
    line_len: &mut usize,
    line_ends_with_cr: &mut bool,
) -> Result<(), ToolOutputError> {
    while let Some(newline) = text.find('\n') {
        let segment = &text[..newline];
        if !segment.is_empty() {
            sink.write(segment, *line_len);
            *line_len += segment.len();
            *line_ends_with_cr = segment.ends_with('\r');
        }
        let logical_len = line_len.saturating_sub(usize::from(*line_ends_with_cr));
        sink.finish_line(
            LineMeta {
                index: *total_lines,
                start: *line_start,
                len: logical_len,
            },
            *line_ends_with_cr,
        )?;
        *total_lines += 1;
        *line_start = line_start.saturating_add(*line_len).saturating_add(1);
        *line_len = 0;
        *line_ends_with_cr = false;
        text = &text[newline + 1..];
    }
    if !text.is_empty() {
        sink.write(text, *line_len);
        *line_len += text.len();
        *line_ends_with_cr = text.ends_with('\r');
    }
    Ok(())
}

struct ReadScan {
    start: usize,
    limit: usize,
    byte_offset: usize,
    line_index: usize,
    page: String,
    current: Vec<u8>,
    current_boundary: Option<bool>,
    returned_lines: usize,
    next_offset: Option<usize>,
    next_byte_offset: usize,
    error: Option<ToolOutputError>,
}

impl ReadScan {
    fn new(start: usize, limit: usize, byte_offset: usize) -> Self {
        Self {
            start,
            limit,
            byte_offset,
            line_index: 0,
            page: String::new(),
            current: Vec::with_capacity(MAX_DISPLAY_LINE_BYTES.saturating_add(3)),
            current_boundary: None,
            returned_lines: 0,
            next_offset: None,
            next_byte_offset: 0,
            error: None,
        }
    }

    fn finish(&mut self) -> Result<(), ToolOutputError> {
        match self.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl LineSink for ReadScan {
    fn write(&mut self, text: &str, line_offset: usize) {
        if self.error.is_some() || self.next_offset.is_some() || self.returned_lines >= self.limit {
            return;
        }
        if self.line_index < self.start {
            return;
        }
        let selected_offset = if self.line_index == self.start {
            self.byte_offset
        } else {
            0
        };
        if selected_offset >= line_offset && selected_offset < line_offset + text.len() {
            self.current_boundary = Some(text.is_char_boundary(selected_offset - line_offset));
        }
        let separator = usize::from(self.returned_lines > 0);
        let capacity = MAX_READ_BYTES
            .saturating_sub(self.page.len().saturating_add(separator))
            .min(MAX_DISPLAY_LINE_BYTES);
        let end = selected_offset.saturating_add(capacity);
        let chunk_start = selected_offset.max(line_offset);
        let chunk_end = end.min(line_offset + text.len());
        if chunk_start < chunk_end {
            self.current.extend_from_slice(
                &text.as_bytes()[chunk_start - line_offset..chunk_end - line_offset],
            );
        }
    }

    fn finish_line(&mut self, line: LineMeta, _trailing_cr: bool) -> Result<(), ToolOutputError> {
        self.line_index = line.index.saturating_add(1);
        if line.index < self.start {
            return Ok(());
        }
        if self.next_offset.is_some() {
            return Ok(());
        }
        if self.returned_lines >= self.limit {
            self.next_offset = Some(line.index + 1);
            return Ok(());
        }
        let selected_offset = if line.index == self.start {
            self.byte_offset
        } else {
            0
        };
        let valid_boundary = self.current_boundary.unwrap_or(selected_offset == line.len);
        if selected_offset > line.len || !valid_boundary {
            self.error = Some(ToolOutputError::InvalidByteOffset {
                line_number: line.index + 1,
                byte_offset: selected_offset,
            });
            return Ok(());
        }

        let wanted = line
            .len
            .saturating_sub(selected_offset)
            .min(self.current.len());
        self.current.truncate(wanted);
        let valid = std::str::from_utf8(&self.current)
            .map(str::len)
            .unwrap_or_else(|error| error.valid_up_to());
        self.current.truncate(valid);
        let end = selected_offset.saturating_add(valid);
        if valid == 0 && selected_offset < line.len {
            self.next_offset = Some(line.index + 1);
            self.next_byte_offset = selected_offset;
            return Ok(());
        }
        if self.returned_lines > 0 {
            self.page.push('\n');
        }
        self.page
            .push_str(std::str::from_utf8(&self.current).unwrap_or_default());
        self.returned_lines += 1;
        if end < line.len {
            self.next_offset = Some(line.index + 1);
            self.next_byte_offset = end;
        }
        self.current.clear();
        self.current_boundary = None;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct SmallChar {
    bytes: [u8; 4],
    len: usize,
}

impl SmallChar {
    fn new(character: char) -> Self {
        let mut bytes = [0; 4];
        let len = character.encode_utf8(&mut bytes).len();
        Self { bytes, len }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn is_cr(&self) -> bool {
        self.as_bytes() == b"\r"
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveState {
    id: StateID,
    start: usize,
}

struct StreamingRegex<'a> {
    nfa: &'a NFA,
    raw: Vec<ActiveState>,
    active: Vec<ActiveState>,
    next: Vec<ActiveState>,
    stack: Vec<ActiveState>,
    seen: Vec<u32>,
    generation: u32,
    queued: VecDeque<SmallChar>,
    previous: Option<SmallChar>,
    offset: usize,
    matched: Option<Range<usize>>,
}

impl<'a> StreamingRegex<'a> {
    fn new(nfa: &'a NFA) -> Self {
        Self {
            nfa,
            raw: Vec::new(),
            active: Vec::new(),
            next: Vec::new(),
            stack: Vec::new(),
            seen: vec![0; nfa.states().len()],
            generation: 0,
            queued: VecDeque::with_capacity(3),
            previous: None,
            offset: 0,
            matched: None,
        }
    }

    fn reset(&mut self) {
        self.raw.clear();
        self.active.clear();
        self.next.clear();
        self.stack.clear();
        self.queued.clear();
        self.previous = None;
        self.offset = 0;
        self.matched = None;
    }

    fn write(&mut self, text: &str) {
        for character in text.chars() {
            self.queued.push_back(SmallChar::new(character));
            if self.queued.len() >= 3 {
                self.process_front(false);
            }
        }
    }

    fn finish(&mut self, trailing_cr: bool) -> Option<Range<usize>> {
        if trailing_cr && self.queued.back().is_some_and(SmallChar::is_cr) {
            self.queued.pop_back();
        }
        while !self.queued.is_empty() {
            let is_last = self.queued.len() == 1;
            self.process_front(is_last);
        }
        if self.offset == 0 {
            self.prepare_boundary(&[], 0, true, true);
            self.visit_matches();
        }
        self.matched.clone()
    }

    fn process_front(&mut self, is_last: bool) {
        let Some(current) = self.queued.pop_front() else {
            return;
        };
        let next = self.queued.front();
        let previous = self.previous.as_ref().map_or(&[][..], SmallChar::as_bytes);
        let current_bytes = current.as_bytes();
        let next_bytes = next.map_or(&[][..], SmallChar::as_bytes);
        let mut window = [0; 12];
        let mut window_len = 0;
        for bytes in [previous, current_bytes, next_bytes] {
            window[window_len..window_len + bytes.len()].copy_from_slice(bytes);
            window_len += bytes.len();
        }
        let base = previous.len();
        for (index, byte) in current_bytes.iter().copied().enumerate() {
            self.prepare_boundary(&window[..window_len], base + index, index == 0, false);
            self.step(byte);
        }
        if is_last {
            self.prepare_boundary(
                &window[..window_len],
                base + current_bytes.len(),
                true,
                true,
            );
            self.visit_matches();
        }
        self.previous = Some(current);
    }

    fn prepare_boundary(
        &mut self,
        window: &[u8],
        local_at: usize,
        char_boundary: bool,
        is_end: bool,
    ) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.seen.fill(0);
            self.generation = 1;
        }
        self.active.clear();
        let mut raw = std::mem::take(&mut self.raw);
        for state in raw.drain(..) {
            self.add_closure(state, window, local_at, is_end);
        }
        self.raw = raw;
        self.raw.clear();
        if self.matched.is_none() && char_boundary {
            self.add_closure(
                ActiveState {
                    id: self.nfa.start_anchored(),
                    start: self.offset,
                },
                window,
                local_at,
                is_end,
            );
        }
    }

    fn add_closure(&mut self, state: ActiveState, window: &[u8], local_at: usize, is_end: bool) {
        self.stack.push(state);
        while let Some(state) = self.stack.pop() {
            let seen = &mut self.seen[state.id.as_usize()];
            if *seen == self.generation {
                continue;
            }
            *seen = self.generation;
            match self.nfa.state(state.id) {
                State::Fail => {}
                State::Match { .. }
                | State::ByteRange { .. }
                | State::Sparse(_)
                | State::Dense(_) => self.active.push(state),
                State::Look { look, next } => {
                    if look_matches(self.nfa, *look, window, local_at, self.offset, is_end) {
                        self.stack.push(ActiveState {
                            id: *next,
                            start: state.start,
                        });
                    }
                }
                State::Union { alternates } => {
                    self.stack
                        .extend(alternates.iter().rev().map(|id| ActiveState {
                            id: *id,
                            start: state.start,
                        }));
                }
                State::BinaryUnion { alt1, alt2 } => {
                    self.stack.push(ActiveState {
                        id: *alt2,
                        start: state.start,
                    });
                    self.stack.push(ActiveState {
                        id: *alt1,
                        start: state.start,
                    });
                }
                State::Capture { next, .. } => self.stack.push(ActiveState {
                    id: *next,
                    start: state.start,
                }),
            }
        }
    }

    fn step(&mut self, byte: u8) {
        self.next.clear();
        for state in &self.active {
            let next = match self.nfa.state(state.id) {
                State::ByteRange { trans } if trans.matches_byte(byte) => Some(trans.next),
                State::Sparse(transitions) => transitions.matches_byte(byte),
                State::Dense(transitions) => transitions.matches_byte(byte),
                State::Match { .. } => {
                    self.matched = Some(state.start..self.offset);
                    break;
                }
                _ => None,
            };
            if let Some(id) = next {
                self.next.push(ActiveState {
                    id,
                    start: state.start,
                });
            }
        }
        std::mem::swap(&mut self.raw, &mut self.next);
        self.offset += 1;
    }

    fn visit_matches(&mut self) {
        for state in &self.active {
            if matches!(self.nfa.state(state.id), State::Match { .. }) {
                self.matched = Some(state.start..self.offset);
                break;
            }
        }
    }
}

fn look_matches(
    nfa: &NFA,
    look: Look,
    window: &[u8],
    local_at: usize,
    global_at: usize,
    is_end: bool,
) -> bool {
    let before = local_at
        .checked_sub(1)
        .and_then(|index| window.get(index))
        .copied();
    let after = window.get(local_at).copied();
    let line_terminator = nfa.look_matcher().get_line_terminator();
    match look {
        Look::Start => global_at == 0,
        Look::End => is_end,
        Look::StartLF => global_at == 0 || before == Some(line_terminator),
        Look::EndLF => is_end || after == Some(line_terminator),
        Look::StartCRLF => {
            global_at == 0
                || before == Some(b'\n')
                || (before == Some(b'\r') && after != Some(b'\n'))
        }
        Look::EndCRLF => {
            is_end || after == Some(b'\r') || (after == Some(b'\n') && before != Some(b'\r'))
        }
        _ => nfa.look_matcher().matches(look, window, local_at),
    }
}

struct StreamingDfa<'a> {
    dfa: &'a DFA,
    cache: DfaCache,
    state: Option<LazyStateID>,
    offset: usize,
    matched_end: Option<usize>,
    pending: Option<u8>,
    done: bool,
    error: Option<std::io::Error>,
}

impl<'a> StreamingDfa<'a> {
    fn new(regex: &'a HybridRegex) -> Self {
        let dfa = regex.forward();
        Self {
            dfa,
            cache: dfa.create_cache(),
            state: None,
            offset: 0,
            matched_end: None,
            pending: None,
            done: false,
            error: None,
        }
    }

    fn reset(&mut self) {
        self.state = None;
        self.offset = 0;
        self.matched_end = None;
        self.pending = None;
        self.done = false;
        self.error = None;
    }

    fn write(&mut self, text: &str) {
        for byte in text.bytes() {
            if let Some(previous) = self.pending.replace(byte) {
                self.push_byte(previous);
            }
        }
    }

    fn push_byte(&mut self, byte: u8) {
        if self.done || self.error.is_some() {
            return;
        }
        if self.state.is_none() {
            match self.dfa.start_state(&mut self.cache, &StartConfig::new()) {
                Ok(state) => self.state = Some(state),
                Err(error) => {
                    self.error = Some(std::io::Error::other(error.to_string()));
                    return;
                }
            }
        }
        let Some(state) = self.state else {
            return;
        };
        match self.dfa.next_state(&mut self.cache, state, byte) {
            Ok(next) => {
                self.state = Some(next);
                if next.is_match() && byte & 0b1100_0000 != 0b1000_0000 {
                    self.matched_end = Some(self.offset);
                } else if next.is_dead() {
                    self.done = true;
                } else if next.is_quit() {
                    if self.matched_end.is_some() {
                        self.done = true;
                    } else {
                        self.error = Some(std::io::Error::other(
                            "streaming regex entered a quit state",
                        ));
                    }
                }
            }
            Err(error) => {
                self.error = Some(std::io::Error::other(error.to_string()));
            }
        }
        self.offset += 1;
    }

    fn finish(&mut self, trailing_cr: bool) -> Result<Option<Range<usize>>, ToolOutputError> {
        if !trailing_cr && let Some(byte) = self.pending.take() {
            self.push_byte(byte);
        }
        self.pending = None;
        if let Some(error) = self.error.take() {
            return Err(error.into());
        }
        if !self.done {
            if self.state.is_none() {
                self.state = Some(
                    self.dfa
                        .start_state(&mut self.cache, &StartConfig::new())
                        .map_err(|error| std::io::Error::other(error.to_string()))?,
                );
            }
            let state = self.dfa.next_eoi_state(
                &mut self.cache,
                self.state
                    .ok_or_else(|| std::io::Error::other("regex state was not initialized"))?,
            );
            match state {
                Ok(state) if state.is_match() => self.matched_end = Some(self.offset),
                Ok(_) => {}
                Err(error) => return Err(std::io::Error::other(error.to_string()).into()),
            }
        }
        Ok(self.matched_end.map(|end| end..end))
    }
}

enum GrepMatcher<'a> {
    Dfa(StreamingDfa<'a>),
    Nfa(StreamingRegex<'a>),
}

impl GrepMatcher<'_> {
    fn write(&mut self, text: &str) {
        match self {
            Self::Dfa(matcher) => matcher.write(text),
            Self::Nfa(matcher) => matcher.write(text),
        }
    }

    fn finish(&mut self, trailing_cr: bool) -> Result<Option<Range<usize>>, ToolOutputError> {
        match self {
            Self::Dfa(matcher) => matcher.finish(trailing_cr),
            Self::Nfa(matcher) => Ok(matcher.finish(trailing_cr)),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Dfa(matcher) => matcher.reset(),
            Self::Nfa(matcher) => matcher.reset(),
        }
    }
}

struct GrepScan<'a> {
    matcher: GrepMatcher<'a>,
    start: usize,
    limit: usize,
    before: usize,
    after: usize,
    previous: VecDeque<LineMeta>,
    remaining_after: usize,
    rows: Vec<PendingGrepRow>,
    matches: Vec<MatchingLine>,
    next_match: Option<MatchingLine>,
    scan_line: bool,
}

impl<'a> GrepScan<'a> {
    fn new(
        nfa: &'a NFA,
        hybrid: Option<&'a HybridRegex>,
        start: usize,
        limit: usize,
        before: usize,
        after: usize,
    ) -> Self {
        Self {
            matcher: hybrid.map_or_else(
                || GrepMatcher::Nfa(StreamingRegex::new(nfa)),
                |regex| GrepMatcher::Dfa(StreamingDfa::new(regex)),
            ),
            start,
            limit,
            before,
            after,
            previous: VecDeque::with_capacity(before.saturating_add(1)),
            remaining_after: 0,
            rows: Vec::with_capacity(limit.saturating_mul(before + after + 1)),
            matches: Vec::with_capacity(limit),
            next_match: None,
            scan_line: start == 0,
        }
    }

    fn push_row(&mut self, line: LineMeta, matched: Option<Range<usize>>) {
        if let Some(existing) = self
            .rows
            .iter_mut()
            .rev()
            .find(|row| row.line.index == line.index)
        {
            if matched.is_some() {
                existing.matched = matched;
            }
            return;
        }
        self.rows.push(PendingGrepRow { line, matched });
    }
}

impl LineSink for GrepScan<'_> {
    fn write(&mut self, text: &str, _line_offset: usize) {
        if self.scan_line {
            self.matcher.write(text);
        }
    }

    fn finish_line(&mut self, line: LineMeta, trailing_cr: bool) -> Result<(), ToolOutputError> {
        let matched = if self.scan_line {
            self.matcher.finish(trailing_cr)?
        } else {
            None
        };
        if let Some(range) = matched {
            let matching = MatchingLine {
                index: line.index,
                matched: range.clone(),
            };
            if self.matches.len() >= self.limit {
                self.next_match = Some(matching);
                self.rows.retain(|row| row.line.index < line.index);
            } else {
                let previous = self.previous.iter().copied().collect::<Vec<_>>();
                for context in previous {
                    self.push_row(context, None);
                }
                self.push_row(line, Some(range));
                self.matches.push(matching);
                self.remaining_after = self.after;
            }
        } else if self.next_match.is_none() && self.remaining_after > 0 {
            self.push_row(line, None);
            self.remaining_after -= 1;
        }

        self.previous.push_back(line);
        if self.previous.len() > self.before {
            self.previous.pop_front();
        }
        self.matcher.reset();
        self.scan_line = line.index.saturating_add(1) >= self.start && self.next_match.is_none();
        Ok(())
    }
}

fn resolve_match_starts(
    file: &mut File,
    regex: &HybridRegex,
    rows: &mut [PendingGrepRow],
    matches: &mut [MatchingLine],
) -> Result<(), ToolOutputError> {
    for matching in matches {
        let Some(row) = rows.iter_mut().find(|row| row.line.index == matching.index) else {
            continue;
        };
        let end = matching.matched.end;
        let start = reverse_match_start(file, regex.reverse(), row.line, end)?;
        matching.matched.start = start;
        row.matched = Some(start..end);
    }
    Ok(())
}

fn reverse_match_start(
    file: &mut File,
    dfa: &DFA,
    line: LineMeta,
    end: usize,
) -> Result<usize, ToolOutputError> {
    if end == 0 {
        return Ok(0);
    }
    let following = if end < line.len {
        read_range(file, line.start + end, 1)?.first().copied()
    } else {
        None
    };
    let mut cache = dfa.create_cache();
    let config = StartConfig::new()
        .anchored(Anchored::Yes)
        .look_behind(following);
    let mut state = dfa
        .start_state(&mut cache, &config)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut matched = None;
    let mut position = end;
    let mut buffer = vec![0; SCAN_BUFFER_BYTES.min(end)];
    while position > 0 {
        let start = position.saturating_sub(buffer.len());
        let len = position - start;
        file.seek(SeekFrom::Start((line.start + start) as u64))?;
        file.read_exact(&mut buffer[..len])?;
        for index in (0..len).rev() {
            state = dfa
                .next_state(&mut cache, state, buffer[index])
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if state.is_match() {
                matched = Some(start + index + 1);
            } else if state.is_dead() {
                return matched.ok_or_else(|| {
                    std::io::Error::other("reverse regex did not find the forward match").into()
                });
            } else if state.is_quit() {
                return Err(std::io::Error::other("reverse regex entered a quit state").into());
            }
        }
        position = start;
    }
    state = dfa
        .next_eoi_state(&mut cache, state)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if state.is_match() {
        matched = Some(0);
    }
    matched
        .ok_or_else(|| std::io::Error::other("reverse regex did not find the forward match").into())
}

fn read_grep_rows(
    file: &mut File,
    pending: &[PendingGrepRow],
    matching_lines: &[MatchingLine],
) -> Result<(Vec<ToolOutputGrepRow>, Option<MatchingLine>), ToolOutputError> {
    let mut bounded = Vec::with_capacity(pending.len().min(MAX_GREP_ROWS));
    let mut bytes = 0;
    let mut omitted_after = None;
    for pending_row in pending {
        let line_number = pending_row.line.index + 1;
        let text = read_line_excerpt(file, pending_row.line, pending_row.matched.as_ref())?;
        let row_bytes = decimal_digits(line_number) + 2 + text.len();
        let separator_bytes = usize::from(!bounded.is_empty());
        if bounded.len() >= MAX_GREP_ROWS || bytes + separator_bytes + row_bytes > GREP_ROW_BYTES {
            omitted_after = Some(pending_row.line.index);
            break;
        }
        bytes += separator_bytes + row_bytes;
        bounded.push(ToolOutputGrepRow {
            line_number,
            text,
            is_match: pending_row.matched.is_some(),
        });
    }
    let omitted = omitted_after.and_then(|line_index| {
        matching_lines
            .iter()
            .find(|matching| matching.index >= line_index)
            .cloned()
    });
    Ok((bounded, omitted))
}

fn read_line_excerpt(
    file: &mut File,
    line: LineMeta,
    matched: Option<&Range<usize>>,
) -> std::io::Result<String> {
    if line.len <= MAX_DISPLAY_LINE_BYTES {
        return read_utf8_range(file, line.start, line.len);
    }
    let content_bytes = MAX_DISPLAY_LINE_BYTES.saturating_sub(OMITTED.len() * 2);
    let mut start = matched.map_or(0, |matched| {
        if matched.len() >= content_bytes {
            matched.start
        } else {
            matched
                .start
                .saturating_sub((content_bytes - matched.len()) / 2)
        }
    });
    start = start.min(line.len.saturating_sub(content_bytes));
    start = next_char_boundary(file, line, start)?;
    let mut text = read_utf8_prefix(file, line.start + start, line.len - start, content_bytes)?;
    let mut end = start + text.len();
    if let Some(matched) = matched
        && matched.len() <= content_bytes
        && end < matched.end
    {
        start = next_char_boundary(file, line, matched.end.saturating_sub(content_bytes))?;
        text = read_utf8_prefix(file, line.start + start, line.len - start, content_bytes)?;
        end = start + text.len();
    }
    if start > 0 {
        text.insert_str(0, OMITTED);
    }
    if end < line.len {
        text.push_str(OMITTED);
    }
    Ok(text)
}

fn next_char_boundary(file: &mut File, line: LineMeta, start: usize) -> std::io::Result<usize> {
    let available = (line.len - start).min(4);
    let bytes = read_range(file, line.start + start, available)?;
    Ok(start
        + bytes
            .iter()
            .position(|byte| byte & 0b1100_0000 != 0b1000_0000)
            .unwrap_or(bytes.len()))
}

fn read_utf8_prefix(
    file: &mut File,
    start: usize,
    available: usize,
    max_bytes: usize,
) -> std::io::Result<String> {
    let bytes = read_range(file, start, available.min(max_bytes.saturating_add(3)))?;
    let end = max_bytes.min(bytes.len());
    let valid = std::str::from_utf8(&bytes[..end])
        .map(str::len)
        .unwrap_or_else(|error| error.valid_up_to());
    String::from_utf8(bytes[..valid].to_vec()).map_err(std::io::Error::other)
}

fn read_utf8_range(file: &mut File, start: usize, len: usize) -> std::io::Result<String> {
    String::from_utf8(read_range(file, start, len)?).map_err(std::io::Error::other)
}

fn read_range(file: &mut File, start: usize, len: usize) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start as u64))?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn decimal_digits(mut number: usize) -> usize {
    let mut digits = 1;
    while number >= 10 {
        number /= 10;
        digits += 1;
    }
    digits
}

fn read_head(file: &mut File, max_lines: usize, max_bytes: usize) -> std::io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(max_bytes);
    file.take(max_bytes as u64).read_to_end(&mut bytes)?;
    let valid = std::str::from_utf8(&bytes)
        .map(|text| text.len())
        .unwrap_or_else(|error| error.valid_up_to());
    let text = std::str::from_utf8(&bytes[..valid]).unwrap_or_default();
    Ok(head_lines(text, max_lines).to_owned())
}

fn read_tail(file: &mut File, max_lines: usize, max_bytes: usize) -> std::io::Result<String> {
    if max_lines == 0 || max_bytes == 0 {
        return Ok(String::new());
    }
    let file_len = file.metadata()?.len();
    let read_len = u64::try_from(max_bytes).unwrap_or(u64::MAX).min(file_len);
    file.seek(SeekFrom::Start(file_len - read_len))?;
    let mut bytes = vec![0; read_len as usize];
    file.read_exact(&mut bytes)?;
    let text = (0..=bytes.len().min(3))
        .find_map(|start| std::str::from_utf8(&bytes[start..]).ok())
        .unwrap_or_default();
    Ok(tail_lines(text, max_lines).to_owned())
}

fn head_lines(text: &str, max_lines: usize) -> &str {
    if max_lines == 0 {
        return "";
    }
    let end = text
        .match_indices('\n')
        .nth(max_lines.saturating_sub(1))
        .map_or(text.len(), |(index, _)| index);
    text[..end].trim_end_matches(['\r', '\n'])
}

fn tail_lines(text: &str, max_lines: usize) -> &str {
    if max_lines == 0 {
        return "";
    }
    let logical_end = text.strip_suffix('\n').map_or(text.len(), str::len);
    let text = &text[..logical_end];
    let start = text
        .rmatch_indices('\n')
        .nth(max_lines.saturating_sub(1))
        .map_or(0, |(index, _)| index + 1);
    text[start..].trim_matches(['\r', '\n'])
}

fn collect_references_from_session_file(
    path: &Path,
    output_ids: &mut HashSet<ToolOutputId>,
) -> Option<()> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    if path
        .extension()
        .is_some_and(|extension| extension == "jsonl")
    {
        let reader = BufReader::new(File::open(path).ok()?);
        for line in reader.lines() {
            let line = line.ok()?;
            if !line.is_empty() {
                collect_output_ids(&serde_json::from_str(&line).ok()?, output_ids);
            }
        }
    } else {
        let value = serde_json::from_reader(File::open(path).ok()?).ok()?;
        collect_output_ids(&value, output_ids);
    }
    Some(())
}

fn collect_output_ids(value: &serde_json::Value, output_ids: &mut HashSet<ToolOutputId>) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(id) = object
                .get("output_ref")
                .and_then(|reference| reference.get("id"))
                .and_then(serde_json::Value::as_str)
                .and_then(|id| id.parse().ok())
            {
                output_ids.insert(id);
            }
            if let Some(references) = object
                .get("retained_output_refs")
                .and_then(serde_json::Value::as_array)
            {
                output_ids.extend(references.iter().filter_map(|reference| {
                    reference
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| id.parse::<ToolOutputId>().ok())
                }));
            }
            for value in object.values() {
                collect_output_ids(value, output_ids);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_output_ids(value, output_ids);
            }
        }
        _ => {}
    }
}

fn canonical_session_id(name: &OsStr) -> Option<CaudraId> {
    let name = name.to_str()?;
    let id = name.parse::<CaudraId>().ok()?;
    (id.to_string() == name).then_some(id)
}

fn owned_output_id(name: &OsStr) -> Option<ToolOutputId> {
    let name = name.to_str()?;
    let stem = name.strip_suffix(&format!(".{OUTPUT_EXTENSION}"))?;
    stem.parse::<ToolOutputId>()
        .ok()
        .filter(|id| id.to_string() == stem)
}

fn is_stale(path: &Path, now: SystemTime, grace: Duration) -> Result<bool, ToolOutputError> {
    let modified = fs::symlink_metadata(path)?.modified()?;
    Ok(now.duration_since(modified).is_ok_and(|age| age >= grace))
}

fn remove_artifact(path: &Path, is_dir: bool) -> Result<bool, ToolOutputError> {
    let result = if is_dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::sessions::{Session, SessionDatabase, persisted_session_ids};
    use serde_json::Value;

    const TEST_MAX_BYTES: usize = 1024 * 1024;
    const EXACT_TEXT: &str = "alpha\r\nβeta\n\nend";
    const PAGED_TEXT: &str = "one\ntwo\nthree\nfour\n";
    const GREP_TEXT: &str = "before\nerror 42\nbetween\nerror 7\nafter\n";
    const MISSING_TEXT: &str = "missing";
    const STREAMED_TEXT: &str = "alpha\r\nβeta\n\nend\n";
    const HUGE_LINE_NEEDLE: &str = "needle-at-the-end";
    const READABLE_ID: &str = "brisk-calm-otter";
    const RETRY_ID: &str = "bright-quiet-panda";
    const LEGACY_ID: &str = "CNK1hV6GWoysH3KQMm5wv";
    const LEGACY_REF: &str = r#"{"id":"CNK1hV6GWoysH3KQMm5wv","byte_count":19,"line_count":4}"#;
    const READABLE_REF: &str = r#"{"id":"brisk-calm-otter","byte_count":19,"line_count":4}"#;

    fn test_store() -> (TempDir, ToolOutputStore) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        (
            temp,
            ToolOutputStore::with_max_bytes(state_dir, TEST_MAX_BYTES),
        )
    }

    #[test]
    fn put_persists_exact_utf8_text() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();

        let reference = store.put(session_id, EXACT_TEXT).unwrap();

        assert_eq!(
            fs::read(store.output_path(session_id, reference.id)).unwrap(),
            EXACT_TEXT.as_bytes()
        );
        assert_eq!(reference.byte_count, EXACT_TEXT.len());
        assert_eq!(reference.line_count, line_count(EXACT_TEXT));
    }

    #[test_case("", 0 ; "empty")]
    #[test_case("x", 1 ; "unterminated")]
    #[test_case("x\n", 1 ; "trailing_lf")]
    #[test_case("x\r\n", 1 ; "trailing_crlf")]
    #[test_case("x\n\n", 2 ; "consecutive_blank_lines")]
    fn line_counts_match_managed_output_metadata(text: &str, expected: usize) {
        let (_temp, store) = test_store();
        let reference = store.put(CaudraId::generate(), text).unwrap();

        assert_eq!(line_count(text), expected);
        assert_eq!(reference.line_count, expected);
    }

    #[test]
    fn sink_publishes_exact_appended_utf8_and_metadata() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let mut sink = store.begin(session_id).unwrap();
        let id = sink.id.clone();

        sink.append("alpha\r\n").unwrap();
        sink.append("βeta").unwrap();
        sink.append("\n\nend\n").unwrap();

        assert!(!store.output_path(session_id, id.clone()).exists());
        let reference = sink.finish().unwrap();
        assert_eq!(reference.id, id);
        assert_eq!(reference.byte_count, STREAMED_TEXT.len());
        assert_eq!(reference.line_count, line_count(STREAMED_TEXT));
        assert_eq!(
            fs::read(store.output_path(session_id, id)).unwrap(),
            STREAMED_TEXT.as_bytes()
        );
    }

    #[test]
    fn sink_limit_rejects_append_but_can_publish_accepted_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::with_max_bytes(state_dir, 5);
        let session_id = CaudraId::generate();
        let mut sink = store.begin(session_id).unwrap();

        sink.append("αβ").unwrap();
        let error = sink.append("cd").unwrap_err();
        assert!(matches!(
            error,
            ToolOutputError::TooLarge {
                byte_count: 6,
                max_bytes: 5
            }
        ));

        let reference = sink.finish().unwrap();
        assert_eq!(reference.byte_count, 4);
        assert_eq!(store.load_text(session_id, reference.id).unwrap(), "αβ");
    }

    #[test]
    fn reserved_sink_append_leaves_room_for_control_but_generic_uses_full_cap() {
        const MAX_BYTES: usize = 10;
        const RESERVE_BYTES: usize = 4;

        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::with_max_bytes(state_dir, MAX_BYTES);
        let session_id = CaudraId::generate();
        let mut sink = store.begin(session_id).unwrap();

        sink.append_with_reserve("123456", RESERVE_BYTES).unwrap();
        assert!(matches!(
            sink.append_with_reserve("7", RESERVE_BYTES),
            Err(ToolOutputError::TooLarge {
                byte_count: 7,
                max_bytes: 6,
            })
        ));
        sink.append("7890").unwrap();

        let reference = sink.finish().unwrap();
        assert_eq!(reference.byte_count, MAX_BYTES);
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            "1234567890"
        );
    }

    #[test]
    fn put_retains_the_full_configured_limit() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::with_max_bytes(state_dir, 4);

        let reference = store.put(CaudraId::generate(), "1234").unwrap();

        assert_eq!(reference.byte_count, 4);
    }

    #[test]
    fn sink_finish_atomically_replaces_temporary_state() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let mut sink = store.begin(session_id).unwrap();
        let id = sink.id.clone();
        let temp_path = sink.file.as_ref().unwrap().path().to_path_buf();
        sink.append("accepted").unwrap();

        assert!(temp_path.exists());
        assert!(!store.output_path(session_id, id.clone()).exists());

        sink.finish().unwrap();
        assert!(!temp_path.exists());
        assert_eq!(
            fs::read(store.output_path(session_id, id)).unwrap(),
            b"accepted"
        );
    }

    #[test]
    fn sink_discard_removes_temporary_state_without_publishing() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let mut sink = store.begin(session_id).unwrap();
        let id = sink.id.clone();
        let temp_path = sink.file.as_ref().unwrap().path().to_path_buf();
        sink.append("discarded").unwrap();

        sink.discard().unwrap();

        assert!(!temp_path.exists());
        assert!(!store.output_path(session_id, id).exists());
    }

    #[test]
    fn sink_drop_removes_temporary_state_without_publishing() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let (id, temp_path) = {
            let mut sink = store.begin(session_id).unwrap();
            sink.append("dropped").unwrap();
            (
                sink.id.clone(),
                sink.file.as_ref().unwrap().path().to_path_buf(),
            )
        };

        assert!(!temp_path.exists());
        assert!(!store.output_path(session_id, id).exists());
    }

    #[test]
    fn load_text_enforces_session_ownership() {
        let (_temp, store) = test_store();
        let owner = CaudraId::generate();
        let other = CaudraId::generate();
        let reference = store.put(owner, EXACT_TEXT).unwrap();

        assert_eq!(
            store.load_text(owner, reference.id.clone()).unwrap(),
            EXACT_TEXT
        );
        assert!(matches!(
            store.load_text(other, reference.id.clone()),
            Err(ToolOutputError::NotFound {
                session_id,
                output_id
            }) if session_id == other && output_id == reference.id
        ));
    }

    #[test]
    fn outputs_are_isolated_by_session() {
        let (_temp, store) = test_store();
        let owner = CaudraId::generate();
        let other = CaudraId::generate();
        let reference = store.put(owner, EXACT_TEXT).unwrap();

        let error = store.read(other, reference.id, 1, 1).unwrap_err();

        assert!(matches!(error, ToolOutputError::NotFound { .. }));
    }

    #[test]
    fn read_respects_utf8_line_and_page_byte_caps() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let line = "é".repeat(MAX_DISPLAY_LINE_BYTES / 2);
        let text = std::iter::repeat_n(line, 30).collect::<Vec<_>>().join("\n");
        let reference = store.put(session_id, &text).unwrap();

        let result = store
            .read(session_id, reference.id, 1, MAX_READ_LINES)
            .unwrap();

        assert!(result.text.len() <= MAX_READ_BYTES);
        assert!(result.text.is_char_boundary(result.text.len()));
        assert!(
            result
                .text
                .lines()
                .all(|displayed| displayed.len() <= MAX_DISPLAY_LINE_BYTES)
        );
        assert_eq!(result.next_offset, Some(result.returned_lines));
        assert!(result.next_byte_offset > 0);
    }

    #[test]
    fn read_at_round_trips_a_long_utf8_line_without_skipping_bytes() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let text = format!("start-{}-end", "蟹".repeat(2_000));
        let reference = store.put(session_id, &text).unwrap();
        let mut offset = 1;
        let mut byte_offset = 0;
        let mut restored = String::new();

        loop {
            let page = store
                .read_at(session_id, reference.id.clone(), offset, 1, byte_offset)
                .unwrap();
            restored.push_str(&page.text);
            let Some(next_offset) = page.next_offset else {
                break;
            };
            assert_eq!(next_offset, 1);
            assert!(page.next_byte_offset > byte_offset);
            assert!(text.is_char_boundary(page.next_byte_offset));
            offset = next_offset;
            byte_offset = page.next_byte_offset;
        }

        assert_eq!(restored, text);
    }

    #[test]
    fn read_and_grep_stream_a_maximum_sized_line() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::new(state_dir);
        let session_id = CaudraId::generate();
        let mut sink = store.begin(session_id).unwrap();
        let chunk = "a".repeat(1024 * 1024);
        let prefix_bytes = DEFAULT_MAX_STORED_BYTES - HUGE_LINE_NEEDLE.len();
        for _ in 0..prefix_bytes / chunk.len() {
            sink.append(&chunk).unwrap();
        }
        sink.append(&"a".repeat(prefix_bytes % chunk.len()))
            .unwrap();
        sink.append(HUGE_LINE_NEEDLE).unwrap();
        let reference = sink.finish().unwrap();

        let read = store
            .read_at(session_id, reference.id.clone(), 1, 1, prefix_bytes)
            .unwrap();
        let grep = store
            .grep(session_id, reference.id, HUGE_LINE_NEEDLE, 1, 1, 0, 0)
            .unwrap();

        assert_eq!(reference.byte_count, DEFAULT_MAX_STORED_BYTES);
        assert_eq!(reference.line_count, 1);
        assert_eq!(read.text, HUGE_LINE_NEEDLE);
        assert_eq!(read.total_bytes, DEFAULT_MAX_STORED_BYTES);
        assert_eq!(read.total_lines, 1);
        assert_eq!(grep.rows.len(), 1);
        assert!(grep.rows[0].text.contains(HUGE_LINE_NEEDLE));
    }

    #[test]
    fn read_paginates_with_one_indexed_offsets() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let reference = store.put(session_id, PAGED_TEXT).unwrap();

        let first = store.read(session_id, reference.id.clone(), 2, 2).unwrap();
        let second = store
            .read(session_id, reference.id, first.next_offset.unwrap(), 2)
            .unwrap();

        assert_eq!(first.text, "two\nthree");
        assert_eq!(first.returned_lines, 2);
        assert_eq!(first.next_offset, Some(4));
        assert_eq!(first.total_lines, 4);
        assert_eq!(first.total_bytes, PAGED_TEXT.len());
        assert_eq!(second.text, "four");
        assert_eq!(second.next_offset, None);
    }

    #[test]
    fn grep_supports_regex_context_and_pagination() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let reference = store.put(session_id, GREP_TEXT).unwrap();

        let first = store
            .grep(session_id, reference.id.clone(), r"error \d+", 1, 1, 1, 1)
            .unwrap();
        let second = store
            .grep(
                session_id,
                reference.id,
                r"error \d+",
                first.next_offset.unwrap(),
                1,
                1,
                1,
            )
            .unwrap();

        assert_eq!(
            first
                .rows
                .iter()
                .map(|row| (row.line_number, row.is_match))
                .collect::<Vec<_>>(),
            vec![(1, false), (2, true), (3, false)]
        );
        assert_eq!(first.next_offset, Some(4));
        assert_eq!(
            second
                .rows
                .iter()
                .map(|row| (row.line_number, row.is_match))
                .collect::<Vec<_>>(),
            vec![(3, false), (4, true), (5, false)]
        );
        assert_eq!(second.next_offset, None);
    }

    #[test_case(r"abc|a", "abc", Some(0..3) ; "leftmost_alternation")]
    #[test_case(r"a+", "zaaaaz", Some(1..5) ; "greedy")]
    #[test_case(r"a+?", "zaaaaz", Some(1..2) ; "lazy")]
    #[test_case(r"\bβeta\b", "x βeta y", Some(2..7) ; "unicode_word_boundary")]
    #[test_case(r"^$", "", Some(0..0) ; "empty_anchored")]
    #[test_case(r"(?m)^b", "a\rb", None ; "lf_anchor_ignores_carriage_return")]
    #[test_case(r"(?mR)^b", "a\rb", Some(2..3) ; "crlf_anchor_accepts_lone_carriage_return")]
    fn streaming_regex_matches_regex_crate(
        pattern: &str,
        text: &str,
        expected: Option<Range<usize>>,
    ) {
        let regex = Regex::new(pattern).unwrap();
        let nfa = NFA::new(pattern).unwrap();
        let mut streamed = StreamingRegex::new(&nfa);
        for character in text.chars() {
            streamed.write(&character.to_string());
        }

        assert_eq!(regex.find(text).map(|matched| matched.range()), expected);
        assert_eq!(streamed.finish(false), expected);
        if !nfa.look_set_any().contains_word_unicode() {
            let hybrid = HybridRegex::new(pattern).unwrap();
            let mut streamed = StreamingDfa::new(&hybrid);
            for character in text.chars() {
                streamed.write(&character.to_string());
            }
            let mut matched = streamed.finish(false).unwrap();
            if let Some(range) = &mut matched {
                let mut file = tempfile::tempfile().unwrap();
                file.write_all(text.as_bytes()).unwrap();
                range.start = reverse_match_start(
                    &mut file,
                    hybrid.reverse(),
                    LineMeta {
                        index: 0,
                        start: 0,
                        len: text.len(),
                    },
                    range.end,
                )
                .unwrap();
            }
            assert_eq!(matched, expected);
        }
    }

    #[test]
    fn read_and_grep_validate_utf8_beyond_requested_rows() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let reference = store.put(session_id, "visible\nhidden").unwrap();
        let path = store.output_path(session_id, reference.id.clone());
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() = 0xff;
        fs::write(path, bytes).unwrap();

        assert!(matches!(
            store.read(session_id, reference.id.clone(), 1, 1),
            Err(ToolOutputError::InvalidUtf8 { .. })
        ));
        assert!(matches!(
            store.grep(session_id, reference.id, "visible", 1, 1, 0, 0),
            Err(ToolOutputError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn grep_excerpt_contains_a_match_beyond_the_display_prefix() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let text = format!("{}needle{}", "a".repeat(3_000), "b".repeat(3_000));
        let reference = store.put(session_id, &text).unwrap();

        let result = store
            .grep(session_id, reference.id, "needle", 1, 1, 0, 0)
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert!(result.rows[0].text.contains("needle"));
        assert!(result.rows[0].text.starts_with(OMITTED));
        assert!(result.rows[0].text.ends_with(OMITTED));
        assert!(result.rows[0].text.len() <= MAX_DISPLAY_LINE_BYTES);
    }

    #[test]
    fn grep_aggregate_is_bounded_and_next_offset_replays_first_omitted_match() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let text = (1..=MAX_GREP_MATCHES)
            .map(|line| format!("match-{line}-{}", "x".repeat(MAX_DISPLAY_LINE_BYTES)))
            .collect::<Vec<_>>()
            .join("\n");
        let reference = store.put(session_id, &text).unwrap();

        let first = store
            .grep(
                session_id,
                reference.id.clone(),
                "match-",
                1,
                MAX_GREP_MATCHES,
                0,
                0,
            )
            .unwrap();
        let rendered_bytes = first
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                usize::from(index > 0) + decimal_digits(row.line_number) + 2 + row.text.len()
            })
            .sum::<usize>();
        let next_offset = first.next_offset.unwrap();

        assert!(rendered_bytes <= GREP_ROW_BYTES);
        assert!(first.rows.len() <= MAX_READ_LINES);
        assert_eq!(next_offset, first.rows.len() + 1);

        let second = store
            .grep(
                session_id,
                reference.id,
                "match-",
                next_offset,
                MAX_GREP_MATCHES,
                0,
                0,
            )
            .unwrap();
        assert_eq!(second.rows[0].line_number, next_offset);
        assert!(second.rows[0].is_match);
    }

    #[test]
    fn grep_enforces_the_aggregate_row_cap() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let mut lines = Vec::new();
        for _ in 0..MAX_GREP_MATCHES {
            lines.extend(std::iter::repeat_n("context", MAX_GREP_CONTEXT));
            lines.push("match");
            lines.extend(std::iter::repeat_n("context", MAX_GREP_CONTEXT));
        }
        let reference = store.put(session_id, &lines.join("\n")).unwrap();

        let result = store
            .grep(
                session_id,
                reference.id.clone(),
                "match",
                1,
                MAX_GREP_MATCHES,
                MAX_GREP_CONTEXT,
                MAX_GREP_CONTEXT,
            )
            .unwrap();

        assert!(result.rows.len() <= MAX_READ_LINES);
        let next_offset = result.next_offset.unwrap();
        let next = store
            .grep(session_id, reference.id, "match", next_offset, 1, 0, 0)
            .unwrap();
        assert_eq!(next.rows[0].line_number, next_offset);
    }

    #[test]
    fn preview_reads_only_bounded_utf8_head_and_tail() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let text = format!("HEAD{}TAIL", "蟹".repeat(100_000));
        let reference = store.put(session_id, &text).unwrap();
        let path = store.output_path(session_id, reference.id.clone());
        let mut bytes = fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] = 0xff;
        fs::write(path, bytes).unwrap();

        let preview = store
            .preview(session_id, reference.id.clone(), 8, 240)
            .unwrap();

        assert!(preview.head.starts_with("HEAD"));
        assert!(preview.tail.ends_with("TAIL"));
        assert!(preview.head.len() + preview.tail.len() < 240);
        assert!(matches!(
            store.load_text(session_id, reference.id),
            Err(ToolOutputError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn put_rejects_data_above_configured_max() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::with_max_bytes(state_dir, 4);

        let error = store.put(CaudraId::generate(), "12345").unwrap_err();

        assert!(matches!(error, ToolOutputError::TooLarge { .. }));
        assert!(!store.root().exists());
    }

    #[test]
    fn delete_session_removes_owned_outputs() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let reference = store.put(session_id, EXACT_TEXT).unwrap();

        store.delete_session(session_id).unwrap();

        assert!(!store.session_dir(session_id).exists());
        assert!(matches!(
            store.read(session_id, reference.id, 1, 1),
            Err(ToolOutputError::NotFound { .. })
        ));
    }

    #[test]
    fn copy_session_outputs_preserves_ids_and_text() {
        let (_temp, store) = test_store();
        let source = CaudraId::generate();
        let target = CaudraId::generate();
        let reference = store.put(source, EXACT_TEXT).unwrap();

        store
            .copy_session_outputs(source, target, std::slice::from_ref(&reference))
            .unwrap();

        assert_eq!(
            fs::read(store.output_path(target, reference.id.clone())).unwrap(),
            EXACT_TEXT.as_bytes()
        );
        assert_eq!(
            store
                .read(target, reference.id, 1, MAX_READ_LINES)
                .unwrap()
                .text,
            EXACT_TEXT.lines().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn copy_session_outputs_streams_bytes_without_utf8_loading() {
        let (_temp, store) = test_store();
        let source = CaudraId::generate();
        let target = CaudraId::generate();
        let reference = store.put(source, EXACT_TEXT).unwrap();
        let bytes = vec![0xff; 128 * 1024];
        fs::write(store.output_path(source, reference.id.clone()), &bytes).unwrap();

        store
            .copy_session_outputs(source, target, std::slice::from_ref(&reference))
            .unwrap();

        assert_eq!(
            fs::read(store.output_path(target, reference.id)).unwrap(),
            bytes
        );
    }

    #[test]
    fn copy_session_outputs_rolls_back_new_files_without_touching_preexisting_outputs() {
        let (_temp, store) = test_store();
        let source = CaudraId::generate();
        let target = CaudraId::generate();
        let first = store.put(source, "first").unwrap();
        let second = store.put(source, "second").unwrap();
        fs::create_dir_all(store.session_dir(target)).unwrap();
        let preexisting_path = store.output_path(target, second.id.clone());
        fs::write(&preexisting_path, "preexisting").unwrap();

        let error = store
            .copy_session_outputs(source, target, &[first.clone(), second.clone()])
            .unwrap_err();

        assert!(matches!(
            error,
            ToolOutputError::Io(_) | ToolOutputError::Storage(_)
        ));
        assert!(!store.output_path(target, first.id).exists());
        assert_eq!(fs::read_to_string(preexisting_path).unwrap(), "preexisting");
    }

    #[test]
    fn copy_session_outputs_rejects_a_source_that_grew_over_the_cap() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::with_max_bytes(state_dir, 8);
        let source = CaudraId::generate();
        let target = CaudraId::generate();
        let reference = store.put(source, "source").unwrap();
        fs::write(
            store.output_path(source, reference.id.clone()),
            "source-too-large",
        )
        .unwrap();

        let error = store
            .copy_session_outputs(source, target, std::slice::from_ref(&reference))
            .unwrap_err();

        assert!(matches!(error, ToolOutputError::TooLarge { .. }));
        assert!(!store.session_dir(target).exists());
    }

    #[cfg(unix)]
    #[test]
    fn output_operations_reject_symlinked_artifacts() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = test_store();
        let source = CaudraId::generate();
        let target = CaudraId::generate();
        let reference = store.put(source, "inside").unwrap();
        let outside = store.state_dir.path().join("outside.txt");
        fs::write(&outside, "outside").unwrap();
        let path = store.output_path(source, reference.id.clone());
        fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();

        assert!(store.read(source, reference.id.clone(), 1, 1).is_err());
        assert!(
            store
                .grep(source, reference.id.clone(), "outside", 1, 1, 0, 0)
                .is_err()
        );
        assert!(store.preview(source, reference.id.clone(), 1, 16).is_err());
        assert!(
            store
                .copy_session_outputs(source, target, std::slice::from_ref(&reference))
                .is_err()
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
        assert!(!store.session_dir(target).exists());
    }

    #[cfg(unix)]
    #[test]
    fn output_operations_reject_symlinked_session_directories() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let outside = store.state_dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::create_dir_all(store.root()).unwrap();
        symlink(&outside, store.session_dir(session_id)).unwrap();

        assert!(store.begin(session_id).is_err());
        assert!(store.delete_session(session_id).is_err());
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn output_operations_reject_a_symlinked_output_root() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let outside = store.state_dir.path().join("outside-root");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, store.root()).unwrap();

        assert!(store.begin(session_id).is_err());
        assert!(store.delete_session(session_id).is_err());
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn output_reads_reject_non_regular_artifacts() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let reference = store.put(session_id, "regular").unwrap();
        let path = store.output_path(session_id, reference.id.clone());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();

        assert!(store.read(session_id, reference.id.clone(), 1, 1).is_err());
        assert!(
            store
                .grep(session_id, reference.id, "regular", 1, 1, 0, 0)
                .is_err()
        );
    }

    #[test]
    fn cleanup_keeps_persisted_session_and_removes_stale_orphan() {
        let (temp, mut store) = test_store();
        store.orphan_grace = Duration::ZERO;
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session: Session<Value, Value, Value> = Session::new("model", "/project");
        let live = session.id;
        let orphan = CaudraId::generate();
        let live_output = store.put(live, EXACT_TEXT).unwrap();
        let subagent_output = store.put(live, "subagent output").unwrap();
        let unreferenced_output = store.put(live, "published but unreferenced").unwrap();
        session.push_message(serde_json::json!({"output_ref": live_output}));
        session.set_subagent_messages(
            "task-1".into(),
            vec![serde_json::json!({"output_ref": subagent_output})],
        );
        session.save(&state_dir).unwrap();
        store.put(orphan, EXACT_TEXT).unwrap();
        let temp_path = store.session_dir(live).join(".tmp-stale");
        fs::write(&temp_path, MISSING_TEXT).unwrap();

        let live_session_ids = persisted_session_ids(&state_dir).unwrap();
        let removed = store.cleanup_orphans(&live_session_ids).unwrap();

        assert_eq!(removed, 3);
        assert!(store.output_path(live, live_output.id).exists());
        assert!(store.output_path(live, subagent_output.id).exists());
        assert!(!store.output_path(live, unreferenced_output.id).exists());
        assert!(!store.session_dir(orphan).exists());
        assert!(!temp_path.exists());
    }

    #[test]
    fn cleanup_fails_closed_when_live_session_is_missing_from_database() {
        let (_temp, mut store) = test_store();
        store.orphan_grace = Duration::ZERO;
        let session_id = CaudraId::generate();
        let output = store.put(session_id, EXACT_TEXT).unwrap();

        let removed = store.cleanup_orphans(&[session_id]).unwrap();

        assert_eq!(removed, 0);
        assert!(store.output_path(session_id, output.id).exists());
    }

    #[test]
    fn cleanup_does_not_fall_back_to_standalone_data_after_database_error() {
        let (temp, mut store) = test_store();
        store.orphan_grace = Duration::ZERO;
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let sessions_dir = state_dir
            .ensure_subdir(crate::sessions::SESSIONS_DIR)
            .unwrap();
        let mut session: Session<Value, Value, Value> = Session::new("model", "/project");
        let current = store.put(session.id, "current").unwrap();
        let stale = store.put(session.id, "stale").unwrap();
        session.push_message(serde_json::json!({"output_ref": current}));
        session.save(&state_dir).unwrap();
        fs::write(
            sessions_dir.join(format!("{}.jsonl", session.id)),
            format!(
                "{}\n",
                serde_json::json!({"t": "msg", "d": {"output_ref": stale}})
            ),
        )
        .unwrap();
        let database = state_dir.path().join(crate::sessions::SESSIONS_DB_FILE);
        fs::write(&database, b"corrupt").unwrap();
        let _ = fs::remove_file(format!("{}-wal", database.display()));
        let _ = fs::remove_file(format!("{}-shm", database.display()));

        let removed = store.cleanup_orphans(&[session.id]).unwrap();

        assert_eq!(removed, 0);
        assert!(store.output_path(session.id, current.id).exists());
        assert!(store.output_path(session.id, stale.id).exists());
    }

    #[test]
    fn cleanup_ignores_standalone_session_json() {
        let (temp, mut store) = test_store();
        store.orphan_grace = Duration::ZERO;
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let sessions_dir = state_dir
            .ensure_subdir(crate::sessions::SESSIONS_DIR)
            .unwrap();
        let mut session: Session<Value, Value, Value> = Session::new("model", "/project");
        let referenced = store.put(session.id, "referenced").unwrap();
        let unreferenced = store.put(session.id, "unreferenced").unwrap();
        session.push_message(serde_json::json!({"output_ref": referenced}));
        session.save(&state_dir).unwrap();
        let mut standalone: Session<Value, Value, Value> = Session::new("model", "/project");
        standalone.id = session.id;
        standalone.push_message(serde_json::json!({"output_ref": unreferenced}));
        fs::write(
            sessions_dir.join(format!("{}.json", session.id)),
            serde_json::to_vec(&standalone).unwrap(),
        )
        .unwrap();

        let removed = store.cleanup_orphans(&[session.id]).unwrap();

        assert_eq!(removed, 1);
        assert!(store.output_path(session.id, referenced.id).exists());
        assert!(!store.output_path(session.id, unreferenced.id).exists());
    }

    #[test]
    fn cleanup_keeps_outputs_referenced_only_by_recoverable_archives() {
        let (temp, mut store) = test_store();
        store.orphan_grace = Duration::ZERO;
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session: Session<Value, Value, Value> = Session::new("model", "/project");
        let archived = store.put(session.id, "archived").unwrap();
        let unreferenced = store.put(session.id, "unreferenced").unwrap();
        session.push_message(serde_json::json!({"output_ref": archived}));
        session.push_message(serde_json::json!({"old": true}));
        session.save(&state_dir).unwrap();
        session.replace_messages(vec![serde_json::json!({"summary": true})]);
        session.save(&state_dir).unwrap();

        let removed = store.cleanup_orphans(&[session.id]).unwrap();

        assert_eq!(removed, 1);
        assert!(store.output_path(session.id, archived.id).exists());
        assert!(!store.output_path(session.id, unreferenced.id).exists());
    }

    #[test_case(READABLE_ID; "readable")]
    #[test_case(LEGACY_ID; "legacy")]
    #[test_case("1111111111111111"; "legacy_leading_zeros")]
    fn output_ids_round_trip_exact_canonical_text(raw: &str) {
        let id: ToolOutputId = raw.parse().unwrap();
        assert_eq!(id.as_str(), raw);
        assert_eq!(id.to_string(), raw);
        let json = serde_json::to_string(raw).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), json);
        assert_eq!(serde_json::from_str::<ToolOutputId>(&json).unwrap(), id);
    }

    #[test_case(""; "empty")]
    #[test_case(MISSING_TEXT; "one_word")]
    #[test_case("brisk-calm"; "two_words")]
    #[test_case("brisk-calm-small-otter"; "four_words")]
    #[test_case("-calm-otter"; "empty_first")]
    #[test_case("brisk--otter"; "empty_middle")]
    #[test_case("brisk-calm-"; "empty_last")]
    #[test_case("Brisk-calm-otter"; "uppercase")]
    #[test_case("brisk-calm-otter "; "trailing_space")]
    #[test_case(" brisk-calm-otter"; "leading_space")]
    #[test_case("brisk-calm-otter\n"; "newline")]
    #[test_case("brisk\t-calm-otter"; "tab")]
    #[test_case("brisk-calm-ottér"; "unicode")]
    #[test_case("brisk-calm-otter\0"; "nul")]
    #[test_case("../brisk-calm-otter"; "parent_path")]
    #[test_case("brisk-calm-.."; "parent_component")]
    #[test_case("/brisk-calm-otter"; "absolute_path")]
    #[test_case("brisk/calm/otter"; "slash")]
    #[test_case("brisk\\calm\\otter"; "backslash")]
    #[test_case("brisk-calm-otter.txt"; "extension")]
    #[test_case("brisk-calm-otter1"; "digit")]
    #[test_case(" CNK1hV6GWoysH3KQMm5wv"; "legacy_space")]
    #[test_case("1CNK1hV6GWoysH3KQMm5wv"; "legacy_extra_zero")]
    fn output_ids_reject_invalid_parse_and_deserialization(raw: &str) {
        assert_eq!(raw.parse::<ToolOutputId>(), Err(ToolOutputIdParseError));
        assert!(serde_json::from_value::<ToolOutputId>(Value::String(raw.into())).is_err());
    }

    #[test_case(0, true; "at_bound")]
    #[test_case(1, false; "above_bound")]
    fn output_ids_enforce_length_bound(extra: usize, valid: bool) {
        let raw = format!("{}-b-c", "a".repeat(MAX_OUTPUT_ID_BYTES - 4 + extra));
        assert_eq!(raw.parse::<ToolOutputId>().is_ok(), valid);
        assert_eq!(
            serde_json::from_value::<ToolOutputId>(Value::String(raw)).is_ok(),
            valid
        );
    }

    #[test]
    fn new_outputs_use_shared_wordlist_ids() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let reference = store.put(session_id, EXACT_TEXT).unwrap();
        let words: Vec<_> = reference.id.as_str().split('-').collect();
        assert_eq!(words.len(), OUTPUT_ID_WORDS);
        let adjectives = include_str!("words/adjectives.txt");
        let nouns = include_str!("words/nouns.txt");
        assert!(adjectives.lines().any(|word| word == words[0]));
        assert!(adjectives.lines().any(|word| word == words[1]));
        assert_ne!(words[0], words[1]);
        assert!(nouns.lines().any(|word| word == words[2]));
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            EXACT_TEXT
        );
    }

    #[test_case(false; "retry")]
    #[test_case(true; "exhaustion")]
    fn begin_retries_collisions_before_staging(exhaust: bool) {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let mut first = store
            .begin_with(session_id, || Ok(READABLE_ID.parse().unwrap()))
            .unwrap();
        first.append(EXACT_TEXT).unwrap();
        let reference = first.finish().unwrap();
        let mut attempts = 0;
        let result = store.begin_with(session_id, || {
            attempts += 1;
            Ok(if exhaust || attempts == 1 {
                READABLE_ID
            } else {
                RETRY_ID
            }
            .parse()
            .unwrap())
        });
        if exhaust {
            assert!(matches!(result, Err(ToolOutputError::IdCollision)));
            assert_eq!(attempts, ID_GENERATION_ATTEMPTS);
        } else {
            let sink = result.unwrap();
            assert_eq!(attempts, 2);
            assert_eq!(sink.id.as_str(), RETRY_ID);
            assert!(!store.output_path(session_id, sink.id.clone()).exists());
            sink.discard().unwrap();
        }
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            EXACT_TEXT
        );
        assert_eq!(
            fs::read_dir(store.session_dir(session_id)).unwrap().count(),
            1
        );
    }

    #[test]
    fn finish_collision_exhaustion_preserves_winner_and_cleans_stage() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let mut winner = store
            .begin_with(session_id, || Ok(READABLE_ID.parse().unwrap()))
            .unwrap();
        let mut loser = store
            .begin_with(session_id, || Ok(READABLE_ID.parse().unwrap()))
            .unwrap();
        winner.append(EXACT_TEXT).unwrap();
        loser.append(PAGED_TEXT).unwrap();
        let temp_path = loser.file.as_ref().unwrap().path().to_path_buf();
        let reference = winner.finish().unwrap();
        let mut retries = 0;
        let error = loser
            .finish_with(|| {
                retries += 1;
                Ok(READABLE_ID.parse().unwrap())
            })
            .unwrap_err();
        assert!(matches!(error, ToolOutputError::IdCollision));
        assert_eq!(retries, ID_GENERATION_ATTEMPTS - 1);
        assert!(!temp_path.exists());
        assert_eq!(
            fs::read_dir(store.session_dir(session_id)).unwrap().count(),
            1
        );
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            EXACT_TEXT
        );
    }

    #[test]
    fn concurrent_sinks_retry_publication_without_overwriting() {
        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let barrier = Barrier::new(2);
        let outputs = thread::scope(|scope| {
            let handles: Vec<_> = [EXACT_TEXT, PAGED_TEXT]
                .into_iter()
                .map(|text| {
                    let barrier = &barrier;
                    let store = &store;
                    scope.spawn(move || {
                        let mut sink = store
                            .begin_with(session_id, || Ok(READABLE_ID.parse().unwrap()))
                            .unwrap();
                        sink.append(text).unwrap();
                        barrier.wait();
                        let mut retries = 0;
                        let reference = sink
                            .finish_with(|| {
                                retries += 1;
                                Ok(RETRY_ID.parse().unwrap())
                            })
                            .unwrap();
                        assert_eq!(
                            store.load_text(session_id, reference.id.clone()).unwrap(),
                            text
                        );
                        (reference, retries)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_ne!(outputs[0].0.id, outputs[1].0.id);
        assert_eq!(outputs.iter().map(|(_, retries)| retries).sum::<usize>(), 1);
        assert_eq!(
            fs::read_dir(store.session_dir(session_id)).unwrap().count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn publication_retries_a_dangling_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = test_store();
        let session_id = CaudraId::generate();
        let mut sink = store
            .begin_with(session_id, || Ok(READABLE_ID.parse().unwrap()))
            .unwrap();
        sink.append(EXACT_TEXT).unwrap();
        let path = store.output_path(session_id, sink.id.clone());
        let outside = store.state_dir.path().join(MISSING_TEXT);
        symlink(&outside, &path).unwrap();
        let reference = sink.finish_with(|| Ok(RETRY_ID.parse().unwrap())).unwrap();
        assert_eq!(reference.id.as_str(), RETRY_ID);
        assert_eq!(fs::read_link(path).unwrap(), outside);
        assert!(!outside.exists());
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            EXACT_TEXT
        );
    }

    #[test_case(LEGACY_REF; "legacy")]
    #[test_case(READABLE_REF; "readable")]
    fn persisted_fixtures_reload_copy_lookup_cleanup_and_accounting(fixture: &str) {
        let (_temp, store) = test_store();
        let mut session: Session<Value, Value, Value> = Session::new("model", "/project");
        let reference: ToolOutputRef = serde_json::from_str(fixture).unwrap();
        fs::create_dir_all(store.session_dir(session.id)).unwrap();
        fs::write(
            store.output_path(session.id, reference.id.clone()),
            PAGED_TEXT,
        )
        .unwrap();
        session.push_message(serde_json::json!({"output_ref": reference}));
        session.save(&store.state_dir).unwrap();
        let mut reloaded = ToolOutputStore::new(store.state_dir.clone());
        reloaded.orphan_grace = Duration::ZERO;
        let references = reloaded.referenced_outputs(session.id).unwrap();
        assert!(references.contains(&reference.id));
        let page = reloaded
            .read(session.id, reference.id.clone(), 1, MAX_READ_LINES)
            .unwrap();
        assert_eq!(page.text, PAGED_TEXT.trim_end());
        assert_eq!(page.total_bytes, reference.byte_count);
        assert_eq!(page.total_lines, reference.line_count);
        let grep = reloaded
            .grep(session.id, reference.id.clone(), "two", 1, 1, 0, 0)
            .unwrap();
        assert_eq!(grep.rows[0].line_number, 2);
        let target = CaudraId::generate();
        assert!(matches!(
            reloaded.read(target, reference.id.clone(), 1, 1),
            Err(ToolOutputError::NotFound { .. })
        ));
        assert!(matches!(
            reloaded.grep(target, reference.id.clone(), "two", 1, 1, 0, 0),
            Err(ToolOutputError::NotFound { .. })
        ));
        reloaded
            .copy_session_outputs(session.id, target, &[reference.clone(), reference.clone()])
            .unwrap();
        assert_eq!(
            reloaded.load_text(target, reference.id.clone()).unwrap(),
            PAGED_TEXT
        );
        let database = SessionDatabase::open(&store.state_dir).unwrap();
        assert_eq!(
            database.stats().unwrap().tool_output_file_bytes,
            (PAGED_TEXT.len() * 2) as u64
        );
        assert_eq!(reloaded.count_orphans(&[session.id]).unwrap(), 1);
        assert_eq!(reloaded.cleanup_orphans(&[session.id]).unwrap(), 1);
        assert!(
            reloaded
                .output_path(session.id, reference.id.clone())
                .exists()
        );
        assert!(!reloaded.session_dir(target).exists());
        assert_eq!(
            database.stats().unwrap().tool_output_file_bytes,
            PAGED_TEXT.len() as u64
        );
        reloaded.delete_session(session.id).unwrap();
        assert!(matches!(
            reloaded.load_text(session.id, reference.id),
            Err(ToolOutputError::NotFound { .. })
        ));
    }
}
