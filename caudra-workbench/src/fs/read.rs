//! Reading a file into a buffer, and writing one back.
//!
//! Three things stop a buffer being opened for editing, and all three open a
//! read-only tab instead of failing: a NUL byte, a size past the cap, and bytes
//! that are not UTF-8. Editing any of them would mean saving a lossy
//! reconstruction over the original.

use std::fs;
use std::io::Error as IoError;
#[cfg(not(unix))]
use std::io::Write;
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::str;
use std::time::SystemTime;

#[cfg(unix)]
pub(crate) use anchored::Source;
use memchr::memchr;
#[cfg(not(unix))]
pub(crate) type Source = ();

const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const MAX_EDITABLE_BYTES: u64 = 8 * 1024 * 1024;
const TEMP_PREFIX: &str = ".caudra-tmp-";

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{0} is a directory")]
    IsDirectory(PathBuf),
    #[error("{0} changed since source verification; reopen from its verified locator")]
    StaleSource(PathBuf),
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum LocalSourceError {
    #[error("local source must be an absolute regular-file path without symlinks: {0}")]
    InvalidPath(PathBuf),
    #[error("local source verification failed: {0}")]
    Verification(String),
    #[error("local source changed during verification: {0}")]
    Changed(PathBuf),
    #[error("verified local source editing requires descriptor-relative filesystem operations")]
    Unsupported,
    #[error("local source cannot be edited: {0}")]
    NotEditable(&'static str),
    #[error("cannot read local source {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: IoError,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("{0} was opened read-only")]
    ReadOnly(PathBuf),
    #[error("{0} changed on disk since it was opened")]
    Stale(PathBuf),
    #[error(
        "save may have completed for {path}, but could not be confirmed; reopen and review the source: {source}"
    )]
    Unconfirmed {
        path: PathBuf,
        #[source]
        source: IoError,
    },
    #[error("cannot write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Why a file cannot be edited, which is also what its tab says instead of
/// showing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnly {
    Binary,
    TooLarge,
    NotUtf8,
}

impl ReadOnly {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Binary => "Binary file, not shown",
            Self::TooLarge => "File is too large to open for editing",
            Self::NotUtf8 => "File is not valid UTF-8",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub lines: Vec<String>,
    pub line_ending: LineEnding,
    pub trailing_newline: bool,
    pub read_only: Option<ReadOnly>,
    pub modified: Option<SystemTime>,
}

pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    let metadata = fs::metadata(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.is_dir() {
        return Err(LoadError::IsDirectory(path.to_path_buf()));
    }
    let modified = metadata.modified().ok();
    if metadata.len() > MAX_EDITABLE_BYTES {
        return Ok(rejected(ReadOnly::TooLarge, modified));
    }

    let bytes = fs::read(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if memchr(0, sniff).is_some() {
        return Ok(rejected(ReadOnly::Binary, modified));
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(rejected(ReadOnly::NotUtf8, modified));
    };
    Ok(decode(&text, modified))
}

pub(crate) fn load_local_source(
    path: &Path,
    verify: impl FnOnce(&[u8]) -> Result<(), String>,
) -> Result<(Loaded, Source), LocalSourceError> {
    let io = |source| LocalSourceError::Io {
        path: path.to_path_buf(),
        source,
    };
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(LocalSourceError::InvalidPath(path.to_path_buf()));
    }
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(io)?;
        if metadata.file_type().is_symlink() || (ancestor == path && !metadata.is_file()) {
            return Err(LocalSourceError::InvalidPath(path.to_path_buf()));
        }
    }
    #[cfg(unix)]
    {
        let source = Source::open(path).map_err(io)?;
        if source.metadata.len() > MAX_EDITABLE_BYTES
            || source.bytes.len() as u64 > MAX_EDITABLE_BYTES
        {
            return Err(LocalSourceError::NotEditable(ReadOnly::TooLarge.reason()));
        }
        if memchr(0, &source.bytes).is_some() {
            return Err(LocalSourceError::NotEditable(ReadOnly::Binary.reason()));
        }
        let text = str::from_utf8(&source.bytes)
            .map_err(|_| LocalSourceError::NotEditable(ReadOnly::NotUtf8.reason()))?;
        verify(&source.bytes).map_err(LocalSourceError::Verification)?;
        source
            .validate(path)
            .map_err(|_| LocalSourceError::Changed(path.to_path_buf()))?;
        let loaded = decode(text, source.metadata.modified().ok());
        Ok((loaded, source))
    }
    #[cfg(not(unix))]
    {
        let _ = verify;
        Err(LocalSourceError::Unsupported)
    }
}

pub(crate) fn reload_local_source(source: &Source, path: &Path) -> Result<Loaded, LoadError> {
    #[cfg(unix)]
    {
        source
            .validate(path)
            .map_err(|_| LoadError::StaleSource(path.to_path_buf()))?;
        let text = str::from_utf8(&source.bytes)
            .map_err(|_| LoadError::StaleSource(path.to_path_buf()))?;
        Ok(decode(text, source.metadata.modified().ok()))
    }
    #[cfg(not(unix))]
    {
        let _ = source;
        Err(LoadError::StaleSource(path.to_path_buf()))
    }
}

pub(crate) fn save_local_source(
    source: &mut Source,
    path: &Path,
    contents: &str,
) -> Result<Option<SystemTime>, SaveError> {
    #[cfg(unix)]
    {
        source.save(path, contents)
    }
    #[cfg(not(unix))]
    {
        let _ = (source, contents);
        Err(SaveError::ReadOnly(path.to_path_buf()))
    }
}

fn rejected(read_only: ReadOnly, modified: Option<SystemTime>) -> Loaded {
    Loaded {
        lines: vec![String::new()],
        line_ending: LineEnding::default(),
        trailing_newline: true,
        read_only: Some(read_only),
        modified,
    }
}

/// A file's dominant line ending wins, so a stray `\n` in a CRLF file does not
/// rewrite every line on the next save.
fn decode(text: &str, modified: Option<SystemTime>) -> Loaded {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    let line_ending = if crlf * 2 > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    let trailing_newline = text.ends_with('\n');
    let body = if trailing_newline {
        &text[..text.len() - 1]
    } else {
        text
    };
    let mut lines: Vec<String> = body
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    Loaded {
        lines,
        line_ending,
        trailing_newline,
        read_only: None,
        modified,
    }
}

pub fn encode(lines: &[String], line_ending: LineEnding, trailing_newline: bool) -> String {
    let mut text = lines.join(line_ending.as_str());
    if trailing_newline {
        text.push_str(line_ending.as_str());
    }
    text
}

/// Writes through a sibling temporary file and renames over the original, so a
/// crash mid-write leaves the old contents rather than half the new ones.
///
/// `expected` is the modification time the buffer was loaded at. A mismatch
/// means something else wrote the file, and refusing is the only answer that
/// cannot lose the other writer's work without being asked.
pub fn save(
    path: &Path,
    contents: &str,
    expected: Option<SystemTime>,
) -> Result<Option<SystemTime>, SaveError> {
    #[cfg(unix)]
    {
        anchored::save(path, contents, expected)
    }
    #[cfg(not(unix))]
    {
        if let Some(expected) = expected {
            let current = fs::metadata(path).ok().and_then(|m| m.modified().ok());
            if current.is_some_and(|current| current != expected) {
                return Err(SaveError::Stale(path.to_path_buf()));
            }
        }
        let io = |source| SaveError::Io {
            path: path.to_path_buf(),
            source,
        };
        let directory = path.parent().unwrap_or(Path::new("."));
        let mut file = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(directory)
            .map_err(io)?;
        file.write_all(contents.as_bytes()).map_err(io)?;
        if let Ok(metadata) = fs::metadata(path) {
            file.as_file()
                .set_permissions(metadata.permissions())
                .map_err(io)?;
        }
        file.as_file().sync_all().map_err(io)?;
        let file = file.persist(path).map_err(|error| io(error.error))?;
        Ok(file.metadata().map_err(io)?.modified().ok())
    }
}

#[cfg(unix)]
mod anchored {
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, Metadata, Permissions};
    use std::io::{Error, ErrorKind, Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf, absolute};
    use std::time::SystemTime;

    use rustix::fs::{AtFlags, Mode, OFlags, open, openat, renameat, unlinkat};
    use rustix::io::Errno;

    use super::{MAX_EDITABLE_BYTES, ReadOnly, SaveError, TEMP_PREFIX};

    const ROOT: &str = "/";
    const CHANGED: &str = "local source path or contents changed";
    const TEMP_ATTEMPTS: usize = 16;
    const TEMP_RANDOM_BYTES: usize = 16;
    const TEMP_MODE: Mode = Mode::RUSR.union(Mode::WUSR);
    const TEMP_EXHAUSTED: &str = "could not create a unique workbench temporary file";
    const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const READ_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK)
        .union(OFlags::CLOEXEC);

    #[derive(Debug)]
    struct Parent {
        root: File,
        directories: Vec<(OsString, File)>,
    }

    #[derive(Debug)]
    pub(crate) struct Source {
        path: PathBuf,
        parent: Parent,
        file: File,
        pub(super) metadata: Metadata,
        pub(super) bytes: Vec<u8>,
    }

    struct Temporary<'a> {
        parent: &'a File,
        name: OsString,
        committed: bool,
    }

    impl Drop for Temporary<'_> {
        fn drop(&mut self) {
            if !self.committed {
                let _ = unlinkat(self.parent, &self.name, AtFlags::empty());
            }
        }
    }

    impl Parent {
        fn open(path: &Path) -> Result<Self, Error> {
            let mut parent = Self {
                root: File::from(open(ROOT, DIRECTORY_FLAGS, Mode::empty())?),
                directories: Vec::new(),
            };
            let mut components = path.components();
            if components.next() != Some(Component::RootDir) {
                return Err(changed());
            }
            for component in components {
                let Component::Normal(name) = component else {
                    return Err(changed());
                };
                let file = File::from(openat(
                    parent.directory(),
                    name,
                    DIRECTORY_FLAGS,
                    Mode::empty(),
                )?);
                parent.directories.push((name.to_os_string(), file));
            }
            Ok(parent)
        }

        fn directory(&self) -> &File {
            self.directories.last().map_or(&self.root, |(_, file)| file)
        }

        fn validate(&self) -> Result<(), Error> {
            let mut current = File::from(open(ROOT, DIRECTORY_FLAGS, Mode::empty())?);
            if !same_file(&current.metadata()?, &self.root.metadata()?) {
                return Err(changed());
            }
            for (name, expected) in &self.directories {
                current = File::from(openat(&current, name, DIRECTORY_FLAGS, Mode::empty())?);
                if !same_file(&current.metadata()?, &expected.metadata()?) {
                    return Err(changed());
                }
            }
            Ok(())
        }

        fn read(&self, name: &OsStr) -> Result<File, Error> {
            let file = File::from(openat(self.directory(), name, READ_FLAGS, Mode::empty())?);
            if !file.metadata()?.is_file() {
                return Err(changed());
            }
            Ok(file)
        }

        fn temporary(&self) -> Result<(File, Temporary<'_>), Error> {
            for _ in 0..TEMP_ATTEMPTS {
                let mut random = [0; TEMP_RANDOM_BYTES];
                getrandom::fill(&mut random).map_err(|error| Error::other(error.to_string()))?;
                let name =
                    OsString::from(format!("{TEMP_PREFIX}{:032x}", u128::from_le_bytes(random)));
                match self.create_temporary(&name) {
                    Ok(file) => {
                        return Ok((
                            file,
                            Temporary {
                                parent: self.directory(),
                                name,
                                committed: false,
                            },
                        ));
                    }
                    Err(Errno::EXIST) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(Error::new(ErrorKind::AlreadyExists, TEMP_EXHAUSTED))
        }

        fn create_temporary(&self, name: &OsStr) -> Result<File, Errno> {
            openat(
                self.directory(),
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                TEMP_MODE,
            )
            .map(File::from)
        }

        fn write(
            &self,
            path: &Path,
            contents: &str,
            permissions: Option<Permissions>,
            validate: impl Fn() -> Result<(), SaveError>,
        ) -> Result<(File, Metadata), SaveError> {
            let io = |source| SaveError::Io {
                path: path.to_path_buf(),
                source,
            };
            validate()?;
            self.validate().map_err(io)?;
            let (mut file, mut temporary) = self.temporary().map_err(io)?;
            file.write_all(contents.as_bytes()).map_err(io)?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions).map_err(io)?;
            }
            file.sync_all().map_err(io)?;
            let staged_metadata = file.metadata().map_err(io)?;
            self.validate().map_err(io)?;
            let staged = self.read(&temporary.name).map_err(io)?;
            if !same_revision(&staged_metadata, &file.metadata().map_err(io)?)
                || !same_revision(&staged_metadata, &staged.metadata().map_err(io)?)
            {
                return Err(SaveError::Stale(path.to_path_buf()));
            }
            validate()?;
            self.validate().map_err(io)?;
            renameat(
                self.directory(),
                &temporary.name,
                self.directory(),
                filename(path).map_err(io)?,
            )
            .map_err(|error| io(error.into()))?;
            temporary.committed = true;
            let unconfirmed = |source| SaveError::Unconfirmed {
                path: path.to_path_buf(),
                source,
            };
            self.directory().sync_all().map_err(unconfirmed)?;
            self.validate().map_err(unconfirmed)?;
            let installed = self
                .read(filename(path).map_err(unconfirmed)?)
                .map_err(unconfirmed)?;
            let metadata = file.metadata().map_err(unconfirmed)?;
            if !same_file(&metadata, &installed.metadata().map_err(unconfirmed)?) {
                return Err(unconfirmed(changed()));
            }
            Ok((file, metadata))
        }
    }

    impl Source {
        pub(super) fn open(path: &Path) -> Result<Self, Error> {
            let parent = Parent::open(path.parent().ok_or_else(changed)?)?;
            let mut file = parent.read(filename(path)?)?;
            let metadata = file.metadata()?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(MAX_EDITABLE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if !same_revision(&metadata, &file.metadata()?) {
                return Err(changed());
            }
            Ok(Self {
                path: path.to_path_buf(),
                parent,
                file,
                metadata,
                bytes,
            })
        }

        pub(super) fn validate(&self, path: &Path) -> Result<(), Error> {
            if path != self.path {
                return Err(changed());
            }
            self.parent.validate()?;
            let mut current = self.parent.read(filename(path)?)?;
            if !same_revision(&self.metadata, &self.file.metadata()?)
                || !same_revision(&self.metadata, &current.metadata()?)
            {
                return Err(changed());
            }
            let mut bytes = Vec::new();
            (&mut current)
                .take(MAX_EDITABLE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes != self.bytes || !same_revision(&self.metadata, &current.metadata()?) {
                return Err(changed());
            }
            self.parent.validate()?;
            let current = self.parent.read(filename(path)?)?;
            if !same_revision(&self.metadata, &current.metadata()?) {
                return Err(changed());
            }
            Ok(())
        }

        pub(super) fn save(
            &mut self,
            path: &Path,
            contents: &str,
        ) -> Result<Option<SystemTime>, SaveError> {
            if contents.len() as u64 > MAX_EDITABLE_BYTES {
                return Err(SaveError::Io {
                    path: path.to_path_buf(),
                    source: Error::other(ReadOnly::TooLarge.reason()),
                });
            }
            let (file, metadata) =
                self.parent
                    .write(path, contents, Some(self.metadata.permissions()), || {
                        self.validate(path)
                            .map_err(|_| SaveError::Stale(path.to_path_buf()))
                    })?;
            self.file = file;
            self.metadata = metadata;
            self.bytes = contents.as_bytes().to_vec();
            Ok(self.metadata.modified().ok())
        }
    }

    pub(super) fn save(
        path: &Path,
        contents: &str,
        expected: Option<SystemTime>,
    ) -> Result<Option<SystemTime>, SaveError> {
        let io = |source| SaveError::Io {
            path: path.to_path_buf(),
            source,
        };
        let absolute = absolute(path).map_err(io)?;
        let parent =
            Parent::open(absolute.parent().ok_or_else(changed).map_err(io)?).map_err(io)?;
        let name = filename(&absolute).map_err(io)?;
        let original = match parent.read(name) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == ErrorKind::NotFound && expected.is_none() => None,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(SaveError::Stale(path.to_path_buf()));
            }
            Err(error) => return Err(io(error)),
        };
        let metadata = original
            .as_ref()
            .map(File::metadata)
            .transpose()
            .map_err(io)?;
        if expected.is_some()
            && metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                != expected
        {
            return Err(SaveError::Stale(path.to_path_buf()));
        }
        let (_, metadata) = parent.write(
            &absolute,
            contents,
            metadata.as_ref().map(Metadata::permissions),
            || {
                let current = parent.read(name);
                let unchanged = match (&metadata, current) {
                    (Some(expected), Ok(current)) => current
                        .metadata()
                        .is_ok_and(|current| same_revision(expected, &current)),
                    (None, Err(error)) => error.kind() == ErrorKind::NotFound,
                    _ => false,
                };
                if unchanged {
                    Ok(())
                } else {
                    Err(SaveError::Stale(path.to_path_buf()))
                }
            },
        )?;
        Ok(metadata.modified().ok())
    }

    fn filename(path: &Path) -> Result<&OsStr, Error> {
        path.file_name().ok_or_else(changed)
    }

    fn changed() -> Error {
        Error::new(ErrorKind::InvalidData, CHANGED)
    }

    fn same_file(left: &Metadata, right: &Metadata) -> bool {
        left.dev() == right.dev() && left.ino() == right.ino()
    }

    fn same_revision(left: &Metadata, right: &Metadata) -> bool {
        same_file(left, right)
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }

    #[cfg(test)]
    mod tests {
        use std::cell::Cell;
        use std::ffi::OsStr;
        use std::fs;
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;
        use test_case::test_case;

        use super::{Errno, Parent, SaveError};

        const FILE: &str = "policy.lua";
        const ORIGINAL: &str = "original policy\n";
        const SENTINEL: &str = "unrelated source\n";
        const EDITED: &str = "edited policy\n";
        const PARENT: &str = "config";
        const MOVED: &str = "moved";
        const OUTSIDE: &str = "outside";
        const STAGED: &str = ".caudra-tmp-collision";

        #[test_case(false ; "symlink")]
        #[test_case(true ; "hardlink")]
        fn a_preexisting_staging_entry_is_never_opened_for_writing(hardlink: bool) {
            let dir = TempDir::new().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let sentinel = root.join(FILE);
            let staged = root.join(STAGED);
            fs::write(&sentinel, SENTINEL).unwrap();
            if hardlink {
                fs::hard_link(&sentinel, &staged).unwrap();
            } else {
                symlink(&sentinel, &staged).unwrap();
            }
            let parent = Parent::open(&root).unwrap();
            assert!(matches!(
                parent.create_temporary(OsStr::new(STAGED)),
                Err(Errno::EXIST)
            ));
            assert_eq!(fs::read_to_string(sentinel).unwrap(), SENTINEL);
            assert_eq!(fs::read_to_string(staged).unwrap(), SENTINEL);
        }

        #[test_case(false ; "parent_renamed")]
        #[test_case(true ; "parent_symlinked")]
        fn parent_swap_during_staging_cannot_redirect_writes_or_cleanup(link: bool) {
            let dir = TempDir::new().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let path = root.join(PARENT);
            let moved = root.join(MOVED);
            let outside = root.join(OUTSIDE);
            fs::create_dir(&path).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(path.join(FILE), ORIGINAL).unwrap();
            fs::write(outside.join(FILE), SENTINEL).unwrap();
            let parent = Parent::open(&path).unwrap();
            let staged = Cell::new(false);
            let result = parent.write(&path.join(FILE), EDITED, None, || {
                if staged.replace(true) {
                    fs::rename(&path, &moved).unwrap();
                    if link {
                        symlink(&outside, &path).unwrap();
                    }
                }
                Ok::<(), SaveError>(())
            });
            assert!(result.is_err());
            assert_eq!(fs::read_to_string(moved.join(FILE)).unwrap(), ORIGINAL);
            assert_eq!(fs::read_to_string(outside.join(FILE)).unwrap(), SENTINEL);
            assert_eq!(fs::read_dir(&moved).unwrap().count(), 1);
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LineEnding, LocalSourceError, MAX_EDITABLE_BYTES, ReadOnly, SaveError, encode, load,
        load_local_source, save,
    };
    use std::fs;
    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const ROUND_TRIP: &str = "loading and encoding must reproduce the file byte for byte";
    const REFUSED: &str = "a file that cannot be edited must open read-only, not fail";
    const ATOMIC: &str = "a saved file must contain exactly what was written";
    const STALE: &str = "a file written by someone else must not be overwritten unasked";
    const SOURCE_FILE: &str = "policy.lua";
    #[cfg(unix)]
    const SOURCE_CONTENT: &str = "local policy = {}\n";
    const SOURCE_REFUSED: &str = "an unverified local source must not become an editable document";

    #[cfg(unix)]
    #[test_case(b"\0not text", ReadOnly::Binary ; "binary")]
    #[test_case(b"\xff", ReadOnly::NotUtf8 ; "invalid_utf8")]
    fn local_source_rejects_non_text_before_verification(bytes: &[u8], reason: ReadOnly) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap().join(SOURCE_FILE);
        fs::write(&path, bytes).unwrap();
        let result = load_local_source(&path, |_| panic!("{SOURCE_REFUSED}"));
        assert!(
            matches!(result, Err(LocalSourceError::NotEditable(message)) if message == reason.reason()),
            "{SOURCE_REFUSED}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_source_rejects_oversized_content_before_verification() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap().join(SOURCE_FILE);
        File::create(&path)
            .unwrap()
            .set_len(MAX_EDITABLE_BYTES + 1)
            .unwrap();
        let result = load_local_source(&path, |_| panic!("{SOURCE_REFUSED}"));
        assert!(
            matches!(result, Err(LocalSourceError::NotEditable(message)) if message == ReadOnly::TooLarge.reason()),
            "{SOURCE_REFUSED}"
        );
    }

    #[test_case("policy.lua" ; "relative")]
    #[test_case("remote:policy.lua" ; "remote_label")]
    fn local_source_never_resolves_non_absolute_paths(path: &str) {
        let result = load_local_source(Path::new(path), |_| panic!("{SOURCE_REFUSED}"));
        assert!(matches!(result, Err(LocalSourceError::InvalidPath(_))));
    }

    #[test_case("." ; "directory")]
    #[test_case("missing/../policy.lua" ; "parent_traversal")]
    fn local_source_rejects_unsafe_absolute_paths(relative: &str) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap().join(relative);
        let result = load_local_source(&path, |_| panic!("{SOURCE_REFUSED}"));
        assert!(matches!(result, Err(LocalSourceError::InvalidPath(_))));
    }

    #[cfg(unix)]
    #[test_case(false ; "source_symlink")]
    #[test_case(true ; "ancestor_symlink")]
    fn local_source_refuses_symlinks_before_calling_the_host(ancestor: bool) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(SOURCE_FILE);
        fs::write(&path, SOURCE_CONTENT).unwrap();
        let link = root.join("link");
        let source = if ancestor {
            symlink(&root, &link).unwrap();
            link.join(SOURCE_FILE)
        } else {
            symlink(&path, &link).unwrap();
            link
        };
        let result = load_local_source(&source, |_| panic!("{SOURCE_REFUSED}"));
        assert!(matches!(result, Err(LocalSourceError::InvalidPath(_))));
        assert_eq!(fs::read_to_string(path).unwrap(), SOURCE_CONTENT);
    }

    #[test_case("one\ntwo\nthree\n", LineEnding::Lf ; "lf_with_trailing_newline")]
    #[test_case("one\ntwo", LineEnding::Lf ; "lf_without_trailing_newline")]
    #[test_case("one\r\ntwo\r\n", LineEnding::Crlf ; "crlf")]
    #[test_case("", LineEnding::Lf ; "empty")]
    #[test_case("\n", LineEnding::Lf ; "single_newline")]
    #[test_case("no newline at all", LineEnding::Lf ; "single_line")]
    fn a_loaded_file_encodes_back_to_itself(contents: &str, expected: LineEnding) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.txt");
        fs::write(&path, contents).unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.line_ending, expected);
        assert!(loaded.read_only.is_none());
        assert_eq!(
            encode(&loaded.lines, loaded.line_ending, loaded.trailing_newline),
            contents,
            "{ROUND_TRIP}"
        );
    }

    #[test]
    fn a_mixed_file_keeps_its_dominant_ending() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mixed.txt");
        fs::write(&path, "a\r\nb\r\nc\n").unwrap();
        assert_eq!(load(&path).unwrap().line_ending, LineEnding::Crlf);
    }

    #[test]
    fn a_file_with_a_nul_byte_opens_read_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("binary.bin");
        fs::write(&path, b"\x7fELF\0\0\0\0payload").unwrap();
        assert_eq!(
            load(&path).unwrap().read_only,
            Some(ReadOnly::Binary),
            "{REFUSED}"
        );
    }

    #[test]
    fn invalid_utf8_opens_read_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("latin1.txt");
        fs::write(&path, b"caf\xe9 au lait").unwrap();
        assert_eq!(
            load(&path).unwrap().read_only,
            Some(ReadOnly::NotUtf8),
            "{REFUSED}"
        );
    }

    #[test]
    fn a_file_past_the_cap_opens_read_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("huge.log");
        let big = vec![b'x'; MAX_EDITABLE_BYTES as usize + 1];
        fs::write(&path, big).unwrap();
        assert_eq!(
            load(&path).unwrap().read_only,
            Some(ReadOnly::TooLarge),
            "{REFUSED}"
        );
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let tmp = TempDir::new().unwrap();
        assert!(load(tmp.path()).is_err());
    }

    #[test]
    fn saving_replaces_the_contents_and_leaves_no_temporary_behind() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let stamp = load(&path).unwrap().modified;

        save(&path, "after\n", stamp).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n", "{ATOMIC}");
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("caudra-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{ATOMIC}");
    }

    #[test]
    fn saving_over_someone_elses_write_is_refused() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let stale = Some(std::time::SystemTime::UNIX_EPOCH);

        let error = save(&path, "mine\n", stale).unwrap_err();
        assert!(matches!(error, SaveError::Stale(_)), "{STALE}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "before\n", "{STALE}");
    }

    #[test]
    fn saving_a_new_file_needs_no_stamp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("fresh.txt");
        save(&path, "hello\n", None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n", "{ATOMIC}");
    }

    #[cfg(unix)]
    #[test]
    fn saving_preserves_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("script.sh");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let stamp = load(&path).unwrap().modified;

        save(&path, "#!/bin/sh\necho hi\n", stamp).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "a script must stay executable");
    }
}
