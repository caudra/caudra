#[cfg(target_os = "linux")]
use super::{HELPER_CWD, HELPER_ENV, run_helper};
use super::{require_local, validate_path};
#[cfg(target_os = "linux")]
use crate::dto::{MAX_IMAGE_BYTES, MAX_MIB};
use crate::{Error, Result, dto::Image};
use caudra_config::sandbox::{Revision, SandboxProvider};
#[cfg(target_os = "linux")]
use serde::Deserialize;
use serde::Serialize;
#[cfg(target_os = "linux")]
use serde_json::json;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::{
    ffi::CString,
    fs::File,
    io::{Error as IoError, Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::Path,
    process::Command,
    time::Duration,
};

#[cfg(target_os = "linux")]
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "linux")]
const HEADER_BYTES: usize = 104;
#[cfg(target_os = "linux")]
const QCOW_MAGIC: &[u8] = b"QFI\xfb";
#[cfg(target_os = "linux")]
const MIB: u64 = 1024 * 1024;
#[cfg(target_os = "linux")]
const HASH_BUFFER: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const MAX_HELPER_BYTES: u64 = 128 * MIB;

pub struct ImageProbe {
    pub source_path: PathBuf,
    #[cfg(target_os = "linux")]
    qemu_img: PathBuf,
    #[cfg(target_os = "linux")]
    source: File,
    #[cfg(target_os = "linux")]
    executable: File,
    #[cfg(target_os = "linux")]
    executable_sha256: Revision,
}

#[derive(Debug, Serialize)]
pub struct ProbedImage {
    pub source_path: PathBuf,
    pub qemu_img: PathBuf,
    pub qemu_img_sha256: Revision,
    pub sha256: Revision,
    pub image: Image,
}

impl ImageProbe {
    #[cfg(target_os = "linux")]
    pub fn prepare(
        provider: &SandboxProvider,
        source_path: PathBuf,
        qemu_img: PathBuf,
    ) -> Result<Self> {
        require_local(provider)?;
        let mut source = open_regular(&source_path)?;
        header(&mut source)?;
        let mut executable = open_regular(&qemu_img)?;
        let executable_sha256 = digest(&mut executable, MAX_HELPER_BYTES)?;
        Ok(Self {
            source_path,
            qemu_img,
            source,
            executable,
            executable_sha256,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn prepare(
        provider: &SandboxProvider,
        source_path: PathBuf,
        _qemu_img: PathBuf,
    ) -> Result<Self> {
        require_local(provider)?;
        validate_path(&source_path)?;
        Err(Error::LocalApproval)
    }

    #[cfg(target_os = "linux")]
    fn arguments(&self) -> [String; 5] {
        [
            "info".into(),
            "--output=json".into(),
            "-f".into(),
            "qcow2".into(),
            format!("/proc/self/fd/{}", self.source.as_raw_fd()),
        ]
    }

    pub fn preview(&self) -> Result<String> {
        #[cfg(target_os = "linux")]
        return serde_json::to_string_pretty(&json!({
            "action": "read-only qcow2 metadata probe; no VM or catalog mutation",
            "qemu_img": self.qemu_img,
            "qemu_img_sha256": self.executable_sha256,
            "executable": format!("/proc/self/fd/{}", self.executable.as_raw_fd()),
            "arguments": self.arguments(), "stdin": "",
            "opened_source": self.source_path,
            "cwd": HELPER_CWD, "environment": HELPER_ENV,
            "environment_inheritance": "NONE. Only the reviewed open image and pinned executable descriptors are inherited. No secrets or project executable selectors.",
            "timeout_seconds": PROBE_TIMEOUT.as_secs()
        })).map_err(|_| Error::LocalHelper);
        #[cfg(not(target_os = "linux"))]
        Err(Error::LocalApproval)
    }

    pub fn execute_approved(&self) -> Result<ProbedImage> {
        #[cfg(target_os = "linux")]
        {
            let mut executable = self
                .executable
                .try_clone()
                .map_err(|_| Error::LocalHelper)?;
            let mut source = self.source.try_clone().map_err(|_| Error::LocalHelper)?;
            if digest(&mut executable, MAX_HELPER_BYTES)? != self.executable_sha256 {
                return Err(Error::ReviewChanged);
            }
            let (virtual_size, cluster_size) = header(&mut source)?;
            let sha256 = digest(&mut source, MAX_IMAGE_BYTES)?;
            let source_fd = self.source.as_raw_fd();
            let executable_fd = self.executable.as_raw_fd();
            let mut command = Command::new(format!("/proc/self/fd/{executable_fd}"));
            command.args(self.arguments());
            // These two retained descriptors are the reviewed inputs, not path lookups at execution time.
            unsafe {
                command.pre_exec(move || {
                    for fd in [source_fd, executable_fd] {
                        if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                            return Err(IoError::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            let raw: ProbeOutput =
                serde_json::from_value(run_helper(command, Vec::new(), PROBE_TIMEOUT)?)
                    .map_err(|_| Error::ImageInput("invalid qemu-img metadata"))?;
            if raw.format != "qcow2"
                || raw.virtual_size != virtual_size
                || raw.cluster_size != cluster_size
                || !raw.backing_filename.is_empty()
                || !raw.full_backing_filename.is_empty()
                || raw.encrypted
                || raw.dirty_flag
                || raw.format_specific.data.corrupt
                || !raw.format_specific.data.data_file.is_empty()
            {
                return Err(Error::ImageInput(
                    "only clean, standalone, unencrypted qcow2 images are supported",
                ));
            }
            if digest(&mut source, MAX_IMAGE_BYTES)? != sha256 {
                return Err(Error::ReviewChanged);
            }
            let file_size_bytes = self
                .source
                .metadata()
                .map_err(|_| Error::LocalHelper)?
                .len();
            Ok(ProbedImage {
                source_path: self.source_path.clone(),
                qemu_img: self.qemu_img.clone(),
                qemu_img_sha256: self.executable_sha256.clone(),
                sha256,
                image: Image {
                    format: raw.format,
                    file_size_bytes,
                    virtual_size_bytes: virtual_size,
                    cluster_size,
                    backing_policy: "standalone".into(),
                },
            })
        }
        #[cfg(not(target_os = "linux"))]
        Err(Error::LocalApproval)
    }
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct ProbeOutput {
    format: String,
    virtual_size: u64,
    cluster_size: u64,
    #[serde(default)]
    backing_filename: String,
    #[serde(default)]
    full_backing_filename: String,
    #[serde(default)]
    encrypted: bool,
    #[serde(default)]
    dirty_flag: bool,
    #[serde(default)]
    format_specific: FormatSpecific,
}
#[cfg(target_os = "linux")]
#[derive(Default, Deserialize)]
struct FormatSpecific {
    #[serde(default)]
    data: SpecificData,
}
#[cfg(target_os = "linux")]
#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SpecificData {
    #[serde(default)]
    data_file: String,
    #[serde(default)]
    corrupt: bool,
}

#[cfg(target_os = "linux")]
fn open_regular(path: &Path) -> Result<File> {
    validate_path(path)?;
    let mut file = File::open("/").map_err(|_| Error::LocalHelper)?;
    let parts: Vec<_> = path.iter().skip(1).collect();
    for (index, part) in parts.iter().enumerate() {
        let name = CString::new(part.as_encoded_bytes()).map_err(|_| Error::LocalApproval)?;
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        if index + 1 < parts.len() {
            flags |= libc::O_DIRECTORY;
        }
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd == -1 {
            return Err(Error::ImageInput(
                "path must exist with no symlink components",
            ));
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    if !file.metadata().map_err(|_| Error::LocalHelper)?.is_file() {
        return Err(Error::ImageInput("expected a regular file"));
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn digest(file: &mut File, limit: u64) -> Result<Revision> {
    let before = file.metadata().map_err(|_| Error::LocalHelper)?;
    if before.len() == 0 || before.len() > limit {
        return Err(Error::ImageInput("file size outside probe limits"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| Error::LocalHelper)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; HASH_BUFFER];
    let mut bytes = 0;
    loop {
        let count = file.read(&mut buffer).map_err(|_| Error::LocalHelper)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > limit {
            return Err(Error::LocalHelper);
        }
        hash.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|_| Error::LocalHelper)?;
    if bytes != before.len()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err(Error::ReviewChanged);
    }
    let hex: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Revision::parse(&format!("sha256:{hex}"))?)
}

#[cfg(target_os = "linux")]
fn header(file: &mut File) -> Result<(u64, u64)> {
    let mut bytes = [0; HEADER_BYTES];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|_| Error::ImageInput("qcow2 header unavailable"))?;
    let number = |start: usize, end: usize| {
        bytes[start..end]
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
    };
    let version = number(4, 8);
    let virtual_size = number(24, 32);
    let cluster_bits = number(20, 24);
    if &bytes[..4] != QCOW_MAGIC
        || !matches!(version, 2 | 3)
        || number(8, 16) != 0
        || number(16, 20) != 0
        || number(32, 36) != 0
        || number(60, 64) != 0
        || virtual_size == 0
        || virtual_size > u64::from(MAX_MIB) * MIB
        || !(9..=21).contains(&cluster_bits)
        || (version == 3 && number(72, 80) != 0)
    {
        return Err(Error::ImageInput(
            "only standalone qcow2 v2/v3 without backing, encryption, snapshots or incompatible features is supported",
        ));
    }
    Ok((virtual_size, 1 << cluster_bits))
}

#[cfg(all(test, unix, not(target_os = "linux")))]
mod unsupported_tests {
    use super::ImageProbe;
    use crate::Error;
    use caudra_config::sandbox::{ProviderKind, SandboxOrigin, SandboxProvider};
    use caudra_storage::sandbox_auth::SandboxCredentialRef;
    use test_case::test_case;

    #[test_case("/image.qcow2", true; "valid_path_is_unsupported")]
    #[test_case("relative.qcow2", false; "invalid_path_is_still_validated")]
    fn unsupported_probe_remains_unavailable(source: &str, valid_path: bool) {
        let provider = SandboxProvider {
            kind: ProviderKind::E2bLibvirt,
            api_endpoint: SandboxOrigin::parse("http://127.0.0.1:1").unwrap(),
            proxy_endpoint: SandboxOrigin::parse("http://127.0.0.1:2").unwrap(),
            credential_ref: SandboxCredentialRef::new("test").unwrap(),
        };
        let probe = ImageProbe {
            source_path: source.into(),
        };
        let result = ImageProbe::prepare(&provider, probe.source_path.clone(), "qemu-img".into());
        if valid_path {
            assert!(matches!(result, Err(Error::LocalApproval)));
        } else {
            assert!(matches!(result, Err(Error::ImageInput(_))));
        }
        assert!(matches!(probe.preview(), Err(Error::LocalApproval)));
        assert!(matches!(
            probe.execute_approved(),
            Err(Error::LocalApproval)
        ));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{ImageProbe, MAX_IMAGE_BYTES, MIB, digest};
    use caudra_config::sandbox::{ProviderKind, SandboxOrigin, SandboxProvider};
    use caudra_storage::sandbox_auth::SandboxCredentialRef;
    use serde_json::{Value, json};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };
    use test_case::test_case;

    const INFO: &str = r#"{"format":"qcow2","virtual-size":8388608,"cluster-size":65536}"#;
    const QEMU: &str = "qemu-img pinned";
    const SOURCE: &str = "source with spaces.qcow2";
    const PROBE_SCRIPT: &str = "#!/bin/sh\nset -eu\ntest \"$PWD\" = /\ntest \"$PATH\" = /usr/bin:/bin\ntest -z \"${E2B_LOCAL_QEMU_IMG-}${QEMU_IMG-}${BASH_ENV-}${LD_PRELOAD-}${ANTHROPIC_API_KEY-}${E2B_LOCAL_API_KEY-}\"\ntest \"$1 $2 $3 $4\" = 'info --output=json -f qcow2'\ntest -f \"$5\"\n";

    fn provider() -> SandboxProvider {
        SandboxProvider {
            kind: ProviderKind::E2bLibvirt,
            api_endpoint: SandboxOrigin::parse("http://127.0.0.1:1").unwrap(),
            proxy_endpoint: SandboxOrigin::parse("http://127.0.0.1:2").unwrap(),
            credential_ref: SandboxCredentialRef::new("test").unwrap(),
        }
    }

    fn fixture(root: &Path, info: &str) {
        let mut image = vec![0; 1024];
        image[..4].copy_from_slice(b"QFI\xfb");
        image[4..8].copy_from_slice(&3_u32.to_be_bytes());
        image[20..24].copy_from_slice(&16_u32.to_be_bytes());
        image[24..32].copy_from_slice(&(8 * MIB).to_be_bytes());
        image[100..104].copy_from_slice(&104_u32.to_be_bytes());
        fs::write(root.join(SOURCE), image).unwrap();
        fs::write(
            root.join(QEMU),
            format!("{PROBE_SCRIPT}printf '%s' '{info}'\n"),
        )
        .unwrap();
        fs::set_permissions(root.join(QEMU), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn approved_probe_pins_paths_metadata_and_executable_without_environment() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), INFO);
        let probe = ImageProbe::prepare(
            &provider(),
            temp.path().join(SOURCE),
            temp.path().join(QEMU),
        )
        .unwrap();
        let preview: Value = serde_json::from_str(&probe.preview().unwrap()).unwrap();
        assert_eq!(
            preview["qemu_img"],
            temp.path().join(QEMU).to_str().unwrap()
        );
        assert_eq!(
            preview["opened_source"],
            temp.path().join(SOURCE).to_str().unwrap()
        );
        assert_eq!(preview["stdin"], "");
        assert_eq!(preview["arguments"][0], "info");
        let expected = digest(
            &mut fs::File::open(temp.path().join(SOURCE)).unwrap(),
            MAX_IMAGE_BYTES,
        )
        .unwrap();
        fs::rename(temp.path().join(QEMU), temp.path().join("retained-helper")).unwrap();
        fs::write(temp.path().join(QEMU), "not the approved executable").unwrap();
        let result = probe.execute_approved().unwrap();
        assert_eq!(result.sha256, expected);
        assert_eq!(result.image.virtual_size_bytes, 8 * MIB);
        assert_eq!(result.image.backing_policy, "standalone");
        assert_eq!(preview["qemu_img_sha256"], result.qemu_img_sha256.as_str());
    }

    #[test_case("format", json!("raw"); "unsupported_format")]
    #[test_case("backing-filename", json!("/private/image"); "backing")]
    #[test_case("full-backing-filename", json!("/private/image"); "full_backing")]
    #[test_case("encrypted", json!(true); "encrypted")]
    #[test_case("dirty-flag", json!(true); "dirty")]
    #[test_case("format-specific", json!({"data":{"data-file":"/private/data"}}); "external_data")]
    fn probe_refuses_unsupported_metadata(field: &str, value: Value) {
        let temp = tempfile::tempdir().unwrap();
        let mut info: Value = serde_json::from_str(INFO).unwrap();
        info[field] = value;
        fixture(temp.path(), &info.to_string());
        let probe = ImageProbe::prepare(
            &provider(),
            temp.path().join(SOURCE),
            temp.path().join(QEMU),
        )
        .unwrap();
        assert!(probe.execute_approved().is_err());
    }

    #[test_case("symlink"; "symlink_path")]
    #[test_case("raw"; "raw_image")]
    #[test_case("backing"; "qcow_backing")]
    #[test_case("traversal"; "traversal_path")]
    fn unsafe_input_is_refused_before_spawning(case: &str) {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), INFO);
        let mut path = temp.path().join(SOURCE);
        match case {
            "symlink" => {
                let link = temp.path().join("link.qcow2");
                symlink(&path, &link).unwrap();
                path = link;
            }
            "raw" => fs::write(&path, vec![0; 1024]).unwrap(),
            "backing" => {
                let mut bytes = fs::read(&path).unwrap();
                bytes[8] = 1;
                fs::write(&path, bytes).unwrap();
            }
            _ => path = temp.path().join("unused/../").join(SOURCE),
        }
        assert!(ImageProbe::prepare(&provider(), path, temp.path().join(QEMU)).is_err());
    }
}
