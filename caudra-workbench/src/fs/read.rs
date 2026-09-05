//! Reading a file into a buffer, and writing one back.
//!
//! Three things stop a buffer being opened for editing, and all three open a
//! read-only tab instead of failing: a NUL byte, a size past the cap, and bytes
//! that are not UTF-8. Editing any of them would mean saving a lossy
//! reconstruction over the original.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use memchr::memchr;

const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const MAX_EDITABLE_BYTES: u64 = 8 * 1024 * 1024;
const TEMP_SUFFIX: &str = ".caudra-tmp";

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{0} is a directory")]
    IsDirectory(PathBuf),
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("{0} was opened read-only")]
    ReadOnly(PathBuf),
    #[error("{0} changed on disk since it was opened")]
    Stale(PathBuf),
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
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = directory.join(format!(".{name}{TEMP_SUFFIX}"));

    let mut file = fs::File::create(&temp).map_err(io)?;
    file.write_all(contents.as_bytes()).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);

    if let Ok(metadata) = fs::metadata(path) {
        let _ = fs::set_permissions(&temp, metadata.permissions());
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(io(error));
    }
    Ok(fs::metadata(path).ok().and_then(|m| m.modified().ok()))
}

#[cfg(test)]
mod tests {
    use super::{LineEnding, MAX_EDITABLE_BYTES, ReadOnly, SaveError, encode, load, save};
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    const ROUND_TRIP: &str = "loading and encoding must reproduce the file byte for byte";
    const REFUSED: &str = "a file that cannot be edited must open read-only, not fail";
    const ATOMIC: &str = "a saved file must contain exactly what was written";
    const STALE: &str = "a file written by someone else must not be overwritten unasked";

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
