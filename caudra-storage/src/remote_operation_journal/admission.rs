#[cfg(unix)]
use std::fs::Permissions;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, limits::Limit};
use sha2::{Digest, Sha256};

use super::{
    MAX_DATABASE_BYTES, MAX_SQLITE_VALUE_BYTES, RemoteOperationJournalError, sidecar,
    validate_schema, verify_owner_only,
};
#[cfg(unix)]
use super::{OWNER_DIR_MODE, OWNER_FILE_MODE};

const DATABASE_SUFFIXES: [&str; 4] = ["", "-wal", "-shm", "-journal"];
const MAX_ADMISSION_BYTES: u64 = MAX_DATABASE_BYTES * 4;
const ADMISSION_ATTEMPTS: usize = 3;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const SQLITE_PAGE_SIZE_MAX: u64 = 65536;
const WAL_HEADER_BYTES: u64 = 32;
const WAL_FRAME_BYTES: u64 = 24;
const JOURNAL_MAGIC: &[u8; 8] = b"\xd9\xd5\x05\xf9\x20\xa1\x63\xd7";

struct AdmissionFile {
    metadata: Metadata,
    digest: [u8; 32],
}

pub(super) struct JournalAdmission([Option<AdmissionFile>; DATABASE_SUFFIXES.len()]);

impl JournalAdmission {
    pub(super) fn is_new(&self) -> bool {
        self.0[0].is_none()
    }

    pub(super) fn inspect(path: &Path) -> Result<Self, RemoteOperationJournalError> {
        for _ in 0..ADMISSION_ATTEMPTS {
            match Self::inspect_once(path) {
                Err(RemoteOperationJournalError::AdmissionChanged) => continue,
                result => return result,
            }
        }
        Err(RemoteOperationJournalError::AdmissionChanged)
    }

    // SQLite must never see the originals before admission: even READ_ONLY can
    // modify shared memory, while immutable reads omit committed WAL frames.
    fn inspect_once(path: &Path) -> Result<Self, RemoteOperationJournalError> {
        let mut admission = Self([const { None }; DATABASE_SUFFIXES.len()]);
        let mut total = 0;
        for (entry, suffix) in admission.0.iter_mut().zip(DATABASE_SUFFIXES) {
            if let Some((_, metadata)) = open_source(&sidecar(path, suffix))? {
                total += metadata.len();
                if total > MAX_ADMISSION_BYTES {
                    return Err(RemoteOperationJournalError::JournalFull);
                }
                *entry = Some(AdmissionFile {
                    metadata,
                    digest: [0; 32],
                });
            }
        }
        if admission.is_new() {
            if admission.0[1..].iter().any(Option::is_some) {
                return Err(RemoteOperationJournalError::UnsafeStorage(
                    "SQLite sidecar exists without a database".into(),
                ));
            }
            admission.recheck(path)?;
            return Ok(admission);
        }
        let mut builder = tempfile::Builder::new();
        builder.prefix("caudra-db-admission-");
        #[cfg(unix)]
        builder.permissions(Permissions::from_mode(OWNER_DIR_MODE));
        let temp = builder.tempdir()?;
        let copy_path = temp.path().join("journal.db");
        for (entry, suffix) in admission.0.iter_mut().zip(DATABASE_SUFFIXES) {
            let Some(entry) = entry else { continue };
            let (mut source, metadata) = open_source(&sidecar(path, suffix))?
                .ok_or(RemoteOperationJournalError::AdmissionChanged)?;
            if !same_file(&entry.metadata, &metadata) {
                return Err(RemoteOperationJournalError::AdmissionChanged);
            }
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(OWNER_FILE_MODE);
            let mut target = options.open(sidecar(&copy_path, suffix))?;
            entry.digest = copy_hashed(&mut source, metadata.len(), &mut target)?;
        }
        admission.recheck(path)?;
        // Rebuild the WAL index from committed frames instead of trusting a cache.
        match fs::remove_file(sidecar(&copy_path, "-shm")) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        bound_recovery(&copy_path)?;
        let connection = Connection::open_with_flags(
            &copy_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        validate_schema(&connection, path)?;
        admission.recheck(path)?;
        Ok(admission)
    }

    pub(super) fn recheck(&self, path: &Path) -> Result<(), RemoteOperationJournalError> {
        for (entry, suffix) in self.0.iter().zip(DATABASE_SUFFIXES) {
            let source_path = sidecar(path, suffix);
            match (entry, open_source(&source_path)?) {
                (None, None) => {}
                (Some(entry), Some((mut file, metadata)))
                    if same_file(&entry.metadata, &metadata) =>
                {
                    if copy_hashed(&mut file, metadata.len(), &mut io::sink())? != entry.digest
                        || !same_file(&metadata, &file.metadata()?)
                        || !same_file(&metadata, &fs::symlink_metadata(source_path)?)
                    {
                        return Err(RemoteOperationJournalError::AdmissionChanged);
                    }
                }
                _ => return Err(RemoteOperationJournalError::AdmissionChanged),
            }
        }
        Ok(())
    }

    pub(super) fn lock_existing(self, path: &Path) -> Result<File, RemoteOperationJournalError> {
        let (file, _) = open_source(path)?.ok_or(RemoteOperationJournalError::AdmissionChanged)?;
        file.lock()?;
        // Another accepted opener may have recovered or created sidecars while
        // we waited. Revalidate that state under the cooperating admission lock.
        let accepted = match self.recheck(path) {
            Ok(()) => self,
            Err(RemoteOperationJournalError::AdmissionChanged) => Self::inspect(path)?,
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !accepted.0[0]
            .as_ref()
            .is_some_and(|entry| same_file(&entry.metadata, &metadata))
        {
            return Err(RemoteOperationJournalError::AdmissionChanged);
        }
        Ok(file)
    }
}

fn open_source(path: &Path) -> Result<Option<(File, Metadata)>, RemoteOperationJournalError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(RemoteOperationJournalError::UnsafeStorage(
            "journal input must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(RemoteOperationJournalError::UnsafeStorage(
            "journal input must not have hard links".into(),
        ));
    }
    verify_owner_only(&metadata)?;
    if metadata.len() > MAX_ADMISSION_BYTES {
        return Err(RemoteOperationJournalError::JournalFull);
    }
    Ok(Some((file, metadata)))
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    if a.dev() != b.dev()
        || a.ino() != b.ino()
        || a.mode() != b.mode()
        || a.uid() != b.uid()
        || a.gid() != b.gid()
        || a.nlink() != b.nlink()
        || a.ctime() != b.ctime()
        || a.ctime_nsec() != b.ctime_nsec()
    {
        return false;
    }
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

fn copy_hashed(
    file: &mut File,
    length: u64,
    target: &mut impl Write,
) -> Result<[u8; 32], RemoteOperationJournalError> {
    let mut reader = file.take(length + 1);
    let mut hash = Sha256::new();
    let mut buffer = [0; COPY_BUFFER_BYTES];
    let mut copied = 0;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        copied += n as u64;
        if copied > length {
            return Err(RemoteOperationJournalError::AdmissionChanged);
        }
        target.write_all(&buffer[..n])?;
        hash.update(&buffer[..n]);
    }
    if copied != length {
        return Err(RemoteOperationJournalError::AdmissionChanged);
    }
    Ok(hash.finalize().into())
}

// Bound recovery's output as well as its inputs. Super-journals can name files
// outside the private copy and are not produced by our single-database writes.
fn bound_recovery(path: &Path) -> Result<(), RemoteOperationJournalError> {
    if let Some((mut file, metadata)) = open_source(&sidecar(path, "-journal"))? {
        if metadata.len() >= JOURNAL_MAGIC.len() as u64 {
            file.seek(SeekFrom::End(-(JOURNAL_MAGIC.len() as i64)))?;
            let mut trailer = [0; JOURNAL_MAGIC.len()];
            file.read_exact(&mut trailer)?;
            if &trailer == JOURNAL_MAGIC {
                return Err(RemoteOperationJournalError::UnsafeStorage(
                    "SQLite super-journal is unsupported".into(),
                ));
            }
        }
        file.rewind()?;
        let mut header = [0; WAL_HEADER_BYTES as usize];
        if file.read_exact(&mut header).is_ok() && &header[..8] == JOURNAL_MAGIC {
            let mut page_size = u64::from(be_u32(&header[24..28]));
            if page_size == 0 {
                page_size = SQLITE_PAGE_SIZE_MAX;
            }
            if page_size > SQLITE_PAGE_SIZE_MAX
                || u64::from(be_u32(&header[16..20])) * page_size > MAX_DATABASE_BYTES
            {
                return Err(RemoteOperationJournalError::JournalFull);
            }
        }
    }
    if let Some((mut file, metadata)) = open_source(&sidecar(path, "-wal"))? {
        let mut header = [0; WAL_HEADER_BYTES as usize];
        if file.read_exact(&mut header).is_err() {
            return Ok(());
        }
        let page_size = u64::from(be_u32(&header[8..12]));
        if !(512..=SQLITE_PAGE_SIZE_MAX).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(RemoteOperationJournalError::UnsafeStorage(
                "invalid SQLite WAL page size".into(),
            ));
        }
        let mut offset = WAL_HEADER_BYTES;
        while offset + WAL_FRAME_BYTES + page_size <= metadata.len() {
            file.seek(SeekFrom::Start(offset))?;
            let mut frame = [0; WAL_FRAME_BYTES as usize];
            file.read_exact(&mut frame)?;
            if u64::from(be_u32(&frame[4..8])) * page_size > MAX_DATABASE_BYTES {
                return Err(RemoteOperationJournalError::JournalFull);
            }
            offset += WAL_FRAME_BYTES + page_size;
        }
    }
    Ok(())
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::{self, File, Permissions};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        DATABASE_SUFFIXES, JOURNAL_MAGIC, JournalAdmission, MAX_ADMISSION_BYTES,
        MAX_DATABASE_BYTES, OWNER_FILE_MODE, RemoteOperationJournalError, WAL_FRAME_BYTES,
        WAL_HEADER_BYTES, same_file, sidecar,
    };
    use crate::StateDir;
    use crate::remote_operation_journal::{PAGE_SIZE, RemoteOperationJournal};

    fn fixture() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let journal =
            RemoteOperationJournal::open(&StateDir::from_path(temp.path().to_owned())).unwrap();
        let path = journal.path().to_owned();
        (temp, path)
    }

    #[test_case("replace"; "inode_replacement")]
    #[test_case("write"; "content_change")]
    #[test_case("sidecar"; "new_sidecar")]
    fn recheck_rejects_changed_originals(change: &str) {
        let (_temp, path) = fixture();
        let admission = JournalAdmission::inspect(&path).unwrap();
        match change {
            "replace" => {
                let previous = path.with_extension("previous");
                fs::rename(&path, &previous).unwrap();
                fs::copy(previous, &path).unwrap();
            }
            "write" => {
                let mut contents = fs::read(&path).unwrap();
                contents[60..64].copy_from_slice(&0u32.to_be_bytes());
                fs::write(&path, contents).unwrap();
            }
            "sidecar" => {
                File::create(sidecar(&path, "-journal")).unwrap();
                fs::set_permissions(
                    sidecar(&path, "-journal"),
                    Permissions::from_mode(OWNER_FILE_MODE),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            admission.recheck(&path),
            Err(RemoteOperationJournalError::AdmissionChanged)
        ));
    }

    #[test_case("symlink"; "symlinks")]
    #[test_case("hardlink"; "hard_links")]
    #[test_case("orphan"; "orphan_sidecars")]
    fn unsafe_inputs_are_rejected_without_changes(kind: &str) {
        const CONTENT: &[u8] = b"untouched";
        for suffix in DATABASE_SUFFIXES {
            let (temp, path) = fixture();
            let target = temp.path().join("target");
            fs::write(&target, CONTENT).unwrap();
            fs::set_permissions(&target, Permissions::from_mode(OWNER_FILE_MODE)).unwrap();
            if suffix.is_empty() || kind == "orphan" {
                fs::remove_file(&path).unwrap();
            }
            if suffix.is_empty() && kind == "orphan" {
                continue;
            }
            let input = sidecar(&path, suffix);
            match kind {
                "symlink" => symlink(&target, &input).unwrap(),
                "hardlink" => fs::hard_link(&target, &input).unwrap(),
                "orphan" => {
                    File::create(&input).unwrap();
                    fs::set_permissions(&input, Permissions::from_mode(OWNER_FILE_MODE)).unwrap();
                }
                _ => unreachable!(),
            }
            let before = fs::symlink_metadata(&input).unwrap();
            assert!(JournalAdmission::inspect(&path).is_err());
            assert!(same_file(&before, &fs::symlink_metadata(&input).unwrap()));
            assert_eq!(fs::read(&target).unwrap(), CONTENT);
            if kind == "orphan" {
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn snapshot_input_bytes_are_bounded() {
        for suffix in DATABASE_SUFFIXES {
            let (_temp, path) = fixture();
            let input = sidecar(&path, suffix);
            File::create(&input)
                .unwrap()
                .set_len(MAX_ADMISSION_BYTES + 1)
                .unwrap();
            fs::set_permissions(&input, Permissions::from_mode(OWNER_FILE_MODE)).unwrap();
            let before = fs::metadata(&input).unwrap();
            assert!(matches!(
                JournalAdmission::inspect(&path),
                Err(RemoteOperationJournalError::JournalFull)
            ));
            assert!(same_file(&before, &fs::metadata(&input).unwrap()));
        }
    }

    #[test_case("wal"; "wal_growth")]
    #[test_case("journal"; "rollback_growth")]
    #[test_case("superjournal"; "external_superjournal")]
    fn snapshot_recovery_is_bounded_and_cannot_follow_external_journals(kind: &str) {
        let (_temp, path) = fixture();
        let mut data = vec![0; (WAL_HEADER_BYTES + WAL_FRAME_BYTES + PAGE_SIZE as u64) as usize];
        let oversized_pages = (MAX_DATABASE_BYTES / PAGE_SIZE as u64 + 1) as u32;
        let suffix = match kind {
            "wal" => {
                data[8..12].copy_from_slice(&(PAGE_SIZE as u32).to_be_bytes());
                data[WAL_HEADER_BYTES as usize + 4..WAL_HEADER_BYTES as usize + 8]
                    .copy_from_slice(&oversized_pages.to_be_bytes());
                "-wal"
            }
            "journal" => {
                data[..8].copy_from_slice(JOURNAL_MAGIC);
                data[16..20].copy_from_slice(&oversized_pages.to_be_bytes());
                data[24..28].copy_from_slice(&(PAGE_SIZE as u32).to_be_bytes());
                "-journal"
            }
            "superjournal" => {
                let trailer = data.len() - JOURNAL_MAGIC.len();
                data[trailer..].copy_from_slice(JOURNAL_MAGIC);
                "-journal"
            }
            _ => unreachable!(),
        };
        let input = sidecar(&path, suffix);
        fs::write(&input, &data).unwrap();
        fs::set_permissions(&input, Permissions::from_mode(OWNER_FILE_MODE)).unwrap();
        let before = fs::read(&path).unwrap();
        let result = JournalAdmission::inspect(&path);
        if kind == "superjournal" {
            assert!(matches!(
                result,
                Err(RemoteOperationJournalError::UnsafeStorage(_))
            ));
        } else {
            assert!(matches!(
                result,
                Err(RemoteOperationJournalError::JournalFull)
            ));
        }
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(input).unwrap(), data);
    }
}
