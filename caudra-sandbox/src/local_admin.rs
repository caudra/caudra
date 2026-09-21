use caudra_config::sandbox::{Revision, SandboxName, SandboxProvider};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::{Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};
use url::Url;
use wait_timeout::ChildExt;

use crate::{Error, Result, dto::Manifest};

mod image;
pub use image::{ImageProbe, ProbedImage};

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_HOST_PATH_BYTES: usize = 4096;
const CAUDRA_WORKSPACE_ROOT: &str = "/workspace";
const CAUDRA_SNAPSHOT_ROOT: &str = "/var/lib/workcell-mcp/snapshots";
const CAUDRA_TRANSFER_ROOT: &str = "/var/lib/workcell-mcp/transfers";
const KVM_DEVICE: &str = "/dev/kvm";
const HELPER_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
const HELPER_CWD: &str = "/";
const HELPER_ENV: [(&str, &str); 5] = [
    ("PATH", "/usr/bin:/bin"),
    ("HOME", "/"),
    ("TMPDIR", "/tmp"),
    ("LANG", "C"),
    ("LC_ALL", "C"),
];
pub const MAX_ADMIN_PREVIEW_BYTES: usize = 48 * 1024;

#[derive(Debug, Serialize)]
pub struct LocalDoctor {
    pub kvm_present: bool,
    pub kvm_accessible: bool,
}

pub fn inspect_local(provider: &SandboxProvider) -> Result<LocalDoctor> {
    require_local(provider)?;
    Ok(LocalDoctor {
        kvm_present: PathBuf::from(KVM_DEVICE).exists(),
        kvm_accessible: OpenOptions::new()
            .read(true)
            .write(true)
            .open(KVM_DEVICE)
            .is_ok(),
    })
}

fn require_local(provider: &SandboxProvider) -> Result<()> {
    let url = Url::parse(provider.api_endpoint.as_str()).map_err(|_| Error::LocalApproval)?;
    if !url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    }) {
        return Err(Error::LocalApproval);
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportRequest {
    pub source_path: PathBuf,
    #[serde(rename = "expectedSHA256")]
    pub expected_sha256: Revision,
    pub expected_revision: String,
    pub manifest: Manifest,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildRequest {
    pub recipe: BuildRecipe,
    pub scripts_dir: PathBuf,
    pub expected_revision: String,
    pub manifest: Manifest,
    #[serde(rename = "sourceTemplateID")]
    pub source_template_id: String,
    pub source_revision: String,
    pub workcell_binary: PathBuf,
    pub container_proxy_binary: PathBuf,
    #[serde(rename = "containerProxySHA256")]
    pub container_proxy_sha256: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildRecipe {
    Base,
    Egress,
    Caudra,
}

/// Local administration is a reviewed host process, never an HTTP mutation or a
/// shell fragment. A UI executor must show these arguments and stdin for approval,
/// bound its output/deadline, and keep stderr out of model-visible diagnostics.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalHelper {
    pub executable: PathBuf,
    pub qemu_img: PathBuf,
    pub database: PathBuf,
    pub catalog_dir: PathBuf,
}

pub struct LocalHelperCommand {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub stdin: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "input",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AdminOperation {
    Import(ImportRequest),
    Build(BuildRequest),
    Inspect { id: String, revision: String },
    Gc { id: String, revision: String },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRequest {
    pub helper: LocalHelper,
    pub operation: AdminOperation,
}

impl AdminRequest {
    pub fn command(&self, provider: &SandboxProvider) -> Result<LocalHelperCommand> {
        match &self.operation {
            AdminOperation::Import(request) => self.helper.import(provider, request),
            AdminOperation::Build(request) => self.helper.build(provider, request),
            AdminOperation::Inspect { id, revision } | AdminOperation::Gc { id, revision } => {
                #[derive(Serialize)]
                struct Selection<'a> {
                    id: &'a str,
                    revision: &'a str,
                }
                caudra_config::sandbox::SandboxName::parse(id)?;
                Revision::parse(revision)?;
                self.helper.command(
                    provider,
                    if matches!(self.operation, AdminOperation::Gc { .. }) {
                        "gc"
                    } else {
                        "inspect"
                    },
                    &Selection { id, revision },
                )
            }
        }
    }
}

impl LocalHelperCommand {
    pub fn preview(&self) -> Result<String> {
        let preview = serde_json::to_string_pretty(&serde_json::json!({
            "executable": self.executable,
            "arguments": self.arguments.iter().map(|arg| arg.to_str().ok_or(Error::LocalApproval)).collect::<Result<Vec<_>>>()?,
            "stdin": std::str::from_utf8(&self.stdin).map_err(|_| Error::LocalHelper)?,
            "cwd": HELPER_CWD,
            "environment": HELPER_ENV,
            "environment_inheritance": "NONE. Review the absolute helper and --qemu-img paths as trusted executables. Builds also approve the explicit scriptsDir, workcellBinary and containerProxyBinary in stdin; the trusted helper must verify their recipe/binary digests. Its fixed build launcher is /bin/bash with PATH=/usr/local/bin:/usr/bin:/bin, never a project-provided executable selector.",
            "requirement": "OFFLINE exclusive database/catalog locks. Stop your own daemon separately. Caudra never stops a third-party daemon.",
        })).map_err(|_| Error::LocalHelper)?;
        if preview.len() > MAX_ADMIN_PREVIEW_BYTES {
            return Err(Error::LocalHelper);
        }
        Ok(preview)
    }

    pub fn execute_approved(self) -> Result<Value> {
        self.preview()?;
        let mut command = Command::new(&self.executable);
        command.args(&self.arguments);
        let value = run_helper(command, self.stdin, HELPER_TIMEOUT)?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::LocalHelper);
        }
        Ok(value)
    }
}

fn run_helper(mut command: Command, stdin: Vec<u8>, timeout: Duration) -> Result<Value> {
    command
        .env_clear()
        .envs(HELPER_ENV)
        .current_dir(HELPER_CWD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|_| Error::LocalHelper)?;
    let mut input = child.stdin.take().ok_or(Error::LocalHelper)?;
    let output = child.stdout.take().ok_or(Error::LocalHelper)?;
    let writer = thread::spawn(move || input.write_all(&stdin));
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take(MAX_INPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let status = child.wait_timeout(timeout);
    #[cfg(unix)]
    // This group belongs to this approved helper, never the provider daemon.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    writer
        .join()
        .map_err(|_| Error::LocalHelper)?
        .map_err(|_| Error::LocalHelper)?;
    let bytes = reader
        .join()
        .map_err(|_| Error::LocalHelper)?
        .map_err(|_| Error::LocalHelper)?;
    if !status
        .map_err(|_| Error::LocalHelper)?
        .is_some_and(|status| status.success())
        || bytes.len() > MAX_INPUT_BYTES
    {
        return Err(Error::LocalHelper);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::LocalHelper)
}

impl LocalHelper {
    pub fn import(
        &self,
        provider: &SandboxProvider,
        request: &ImportRequest,
    ) -> Result<LocalHelperCommand> {
        request.manifest.validate()?;
        validate_path(&request.source_path)?;
        optional_revision(&request.expected_revision)?;
        self.command(provider, "import", request)
    }

    pub fn build(
        &self,
        provider: &SandboxProvider,
        request: &BuildRequest,
    ) -> Result<LocalHelperCommand> {
        request.validate()?;
        self.command(provider, "build", request)
    }

    fn command(
        &self,
        provider: &SandboxProvider,
        action: &str,
        request: &impl Serialize,
    ) -> Result<LocalHelperCommand> {
        require_local(provider)?;
        if [
            &self.executable,
            &self.qemu_img,
            &self.database,
            &self.catalog_dir,
        ]
        .iter()
        .any(|path| validate_path(path).is_err())
        {
            return Err(Error::LocalApproval);
        }
        let stdin = serde_json::to_vec(request).map_err(|_| Error::LocalHelper)?;
        if stdin.len() > MAX_INPUT_BYTES {
            return Err(Error::LocalHelper);
        }
        let mut arguments = vec!["templates".into(), action.into()];
        if action != "inspect" {
            arguments.push("--approve".into());
        }
        arguments.extend([
            "--database".into(),
            self.database.clone().into_os_string(),
            "--catalog-dir".into(),
            self.catalog_dir.clone().into_os_string(),
            "--qemu-img".into(),
            self.qemu_img.clone().into_os_string(),
        ]);
        Ok(LocalHelperCommand {
            executable: self.executable.clone(),
            arguments,
            stdin,
        })
    }
}

pub fn validate_path(path: &Path) -> Result<()> {
    let text = path.to_str().ok_or(Error::LocalApproval)?;
    if !path.is_absolute()
        || text == "/"
        || text.len() > MAX_HOST_PATH_BYTES
        || text.contains('\\')
        || text.chars().any(char::is_control)
        || text[1..]
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(Error::ImageInput(
            "host paths must be clean absolute paths without traversal",
        ));
    }
    Ok(())
}

fn optional_revision(revision: &str) -> Result<()> {
    if !revision.is_empty() {
        Revision::parse(revision)?;
    }
    Ok(())
}

impl BuildRequest {
    pub fn validate(&self) -> Result<()> {
        self.manifest.validate()?;
        if self.manifest.guest_ca {
            return Err(Error::ImageInput(
                "fixed build recipes do not provision a MITM CA; import an independently prepared, reviewed CA-compatible image",
            ));
        }
        optional_revision(&self.expected_revision)?;
        validate_path(&self.scripts_dir)?;
        let recipe = match self.recipe {
            BuildRecipe::Base => "base",
            BuildRecipe::Egress => "egress",
            BuildRecipe::Caudra => "caudra",
        };
        if self.manifest.build.recipe != recipe
            || self.manifest.build.source_revision != self.source_revision
        {
            return Err(Error::ImageInput(
                "manifest recipe/source revision must match the build inputs",
            ));
        }
        Revision::parse(&self.manifest.build.recipe_sha256)?;
        if self.recipe == BuildRecipe::Base {
            if !self.source_template_id.is_empty()
                || !self.source_revision.is_empty()
                || self.manifest.network_topology == "slirp-enforced"
                || self.manifest.workcell.workspace_snapshots
            {
                return Err(Error::ImageInput(
                    "base builds take no source and cannot declare egress or snapshot features",
                ));
            }
            validate_path(&self.container_proxy_binary)?;
            Revision::parse(&self.container_proxy_sha256)?;
        } else {
            SandboxName::parse(&self.source_template_id)?;
            Revision::parse(&self.source_revision)?;
            if !self.container_proxy_binary.as_os_str().is_empty()
                || !self.container_proxy_sha256.is_empty()
            {
                return Err(Error::ImageInput(
                    "only base builds take a container proxy binary",
                ));
            }
        }
        if self.recipe == BuildRecipe::Egress {
            if !self.workcell_binary.as_os_str().is_empty()
                || self.manifest.network_topology != "slirp-enforced"
            {
                return Err(Error::ImageInput(
                    "egress builds preserve Workcell and require slirp-enforced topology",
                ));
            }
        } else {
            validate_path(&self.workcell_binary)?;
            Revision::parse(&self.manifest.workcell.sha256)?;
        }
        if self.recipe == BuildRecipe::Caudra
            && (!self.manifest.workcell.remote_workspace
                || !self.manifest.workcell.workspace_snapshots)
        {
            return Err(Error::ImageInput(
                "caudra builds require remote workspace and snapshot features",
            ));
        }
        let workcell = &self.manifest.workcell;
        if self.recipe == BuildRecipe::Caudra
            && ((!workcell.workspace_root.is_empty()
                && workcell.workspace_root != CAUDRA_WORKSPACE_ROOT)
                || (!workcell.snapshot_root.is_empty()
                    && workcell.snapshot_root != CAUDRA_SNAPSHOT_ROOT)
                || (!workcell.transfer_root.is_empty()
                    && workcell.transfer_root != CAUDRA_TRANSFER_ROOT))
        {
            return Err(Error::ImageInput(
                "fixed caudra recipe serves /workspace with /var/lib/workcell-mcp/snapshots and /var/lib/workcell-mcp/transfers; custom roots require a separately built import",
            ));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        AdminOperation, AdminRequest, BuildRecipe, BuildRequest, ImportRequest, LocalHelper,
    };
    use caudra_config::sandbox::{ProviderKind, SandboxOrigin, SandboxProvider};
    use caudra_storage::sandbox_auth::SandboxCredentialRef;
    use serde_json::json;
    use std::{env, fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};
    use test_case::test_case;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HELPER: &str = "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"ok\":true,\"result\":{\"status\":\"inspected\"}}'\n";
    const STATUS: &str = "inspected";
    const DOTENV_CHILD: &str = "CAUDRA_HELPER_DOTENV_TEST";
    const DOTENV_TEST: &str =
        "local_admin::tests::malicious_project_dotenv_never_selects_helper_executables";
    const PROBE: &str = "#!/bin/sh\nset -eu\ntest \"$PWD\" = /\ntest \"$PATH\" = /usr/bin:/bin\ntest -z \"${ANTHROPIC_API_KEY-}${E2B_LOCAL_API_KEY-}${E2B_LOCAL_QEMU_IMG-}${QEMU_IMG-}${BASH_ENV-}${LD_PRELOAD-}${LIBGUESTFS_HV-}\"\nprintf '%s' '{\"ok\":true,\"result\":{\"status\":\"inspected\"}}'\n";
    const ENV_HELPER: &str = "#!/bin/bash\nset -eu\nwhile [[ $# -gt 0 ]]; do\n if [[ $1 == --qemu-img ]]; then qemu_img=$2; shift; fi\n shift\ndone\ncat >/dev/null\nexec \"${E2B_LOCAL_QEMU_IMG:-$qemu_img}\"\n";

    #[test]
    fn malicious_project_dotenv_never_selects_helper_executables() {
        if let Some(root) = env::var_os(DOTENV_CHILD) {
            let root = PathBuf::from(root);
            caudra_config::load_env_files(&root);
            assert!(env::var_os("E2B_LOCAL_QEMU_IMG").is_some());
            let request: AdminRequest =
                serde_json::from_slice(&fs::read(root.join("request.json")).unwrap()).unwrap();
            let provider: SandboxProvider =
                serde_json::from_slice(&fs::read(root.join("provider.json")).unwrap()).unwrap();
            let command = request.command(&provider).unwrap();
            assert!(command.preview().unwrap().contains("--qemu-img"));
            assert_eq!(
                command.execute_approved().unwrap()["result"]["status"],
                STATUS
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let sentinel = root.join("sentinel");
        let malicious = root.join("malicious");
        for (path, content) in [
            (root.join("helper"), ENV_HELPER.to_owned()),
            (root.join("qemu-img"), PROBE.to_owned()),
            (
                malicious.clone(),
                format!(
                    "#!/bin/sh\n/usr/bin/touch '{}'\nexit 90\n",
                    sentinel.display()
                ),
            ),
        ] {
            fs::write(&path, content).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir(root.join(".caudra")).unwrap();
        fs::write(root.join(".caudra/.env"), format!("E2B_LOCAL_QEMU_IMG={}\nQEMU_IMG={}\nBASH_ENV={}\nLIBGUESTFS_HV={}\nPATH={}\nANTHROPIC_API_KEY=fake-provider-secret\nE2B_LOCAL_API_KEY=fake-lifecycle-secret\n", malicious.display(), malicious.display(), malicious.display(), malicious.display(), root.display())).unwrap();
        fs::write(root.join("request.json"), serde_json::to_vec(&json!({"helper":{"executable":root.join("helper"),"qemu_img":root.join("qemu-img"),"database":root.join("db"),"catalog_dir":root.join("catalog")},"operation":{"action":"inspect","input":{"id":"base","revision":DIGEST}}})).unwrap()).unwrap();
        let provider = SandboxProvider {
            kind: ProviderKind::E2bLibvirt,
            api_endpoint: SandboxOrigin::parse("http://127.0.0.1:1").unwrap(),
            proxy_endpoint: SandboxOrigin::parse("http://127.0.0.1:2").unwrap(),
            credential_ref: SandboxCredentialRef::new("test").unwrap(),
        };
        fs::write(
            root.join("provider.json"),
            serde_json::to_vec(&provider).unwrap(),
        )
        .unwrap();
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", DOTENV_TEST, "--nocapture"])
            .env_clear()
            .env(DOTENV_CHILD, root)
            .env("HOME", root)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!sentinel.exists());
    }

    #[test_case("inspect"; "inspect_local")]
    #[test_case("gc"; "gc_requires_approve")]
    #[test_case("import"; "import_requires_approve")]
    #[test_case("build"; "build_requires_approve")]
    fn helper_is_explicit_offline_and_never_http(action: &str) {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("helper");
        fs::write(&executable, HELPER).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = SandboxProvider {
            kind: ProviderKind::E2bLibvirt,
            api_endpoint: SandboxOrigin::parse("http://127.0.0.1:1").unwrap(),
            proxy_endpoint: SandboxOrigin::parse("http://127.0.0.1:2").unwrap(),
            credential_ref: SandboxCredentialRef::new("test").unwrap(),
        };
        let mut manifest: crate::dto::Manifest = serde_json::from_value(json!({"schemaVersion":1,"id":"base","architecture":"x86_64","machine":"q35","minimum":{"cpuCount":1,"memoryMB":512,"diskSizeMB":1024},"defaults":{"cpuCount":2,"memoryMB":1024,"diskSizeMB":1024},"networkTopology":"slirp-unrestricted","workcell":{"version":"test","sha256":"","protocolVersion":"2026-07-28","transferProtocol":"workcell-reviewed-v1","remoteWorkspace":true,"workspaceSnapshots":true,"reviewedTransfer":true},"build":{"recipe":"import","recipeSHA256":"","sourceRevision":""}})).unwrap();
        if action == "build" {
            manifest.build.recipe = "base".into();
            manifest.build.recipe_sha256 = DIGEST.into();
            manifest.workcell.sha256 = DIGEST.into();
            manifest.workcell.workspace_snapshots = false;
        }
        let operation = match action {
            "import" => AdminOperation::Import(ImportRequest {
                source_path: temp.path().join("source.qcow2"),
                expected_sha256: caudra_config::sandbox::Revision::parse(DIGEST).unwrap(),
                expected_revision: String::new(),
                manifest,
            }),
            "build" => AdminOperation::Build(BuildRequest {
                recipe: BuildRecipe::Base,
                scripts_dir: temp.path().join("scripts"),
                expected_revision: String::new(),
                manifest,
                source_template_id: String::new(),
                source_revision: String::new(),
                workcell_binary: temp.path().join("workcell"),
                container_proxy_binary: temp.path().join("proxy"),
                container_proxy_sha256: DIGEST.into(),
            }),
            "gc" => AdminOperation::Gc {
                id: "base".into(),
                revision: DIGEST.into(),
            },
            _ => AdminOperation::Inspect {
                id: "base".into(),
                revision: DIGEST.into(),
            },
        };
        let request = AdminRequest {
            helper: LocalHelper {
                executable,
                qemu_img: temp.path().join("trusted-qemu-img"),
                database: temp.path().join("database"),
                catalog_dir: temp.path().join("catalog"),
            },
            operation,
        };
        let command = request.command(&provider).unwrap();
        assert_eq!(
            command.arguments.iter().any(|arg| arg == "--approve"),
            action != "inspect"
        );
        assert!(command.preview().unwrap().contains("OFFLINE"));
        assert!(!request.helper.database.exists());
        assert_eq!(
            command.execute_approved().unwrap()["result"]["status"],
            STATUS
        );
        let remote = SandboxProvider {
            api_endpoint: SandboxOrigin::parse("https://daemon.example.test").unwrap(),
            ..provider
        };
        assert!(request.command(&remote).is_err());
    }
}
