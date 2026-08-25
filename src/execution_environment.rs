use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command, task::JoinSet};

pub(crate) const EXTENSION_ID: &str = "ai.workcell/execution-environment";

const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_PROBE_OUTPUT_BYTES: u64 = 4_096;
const MAX_METADATA_BYTES: u64 = 64 * 1024;
const INHERITED_ENVIRONMENT: [&str; 13] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "TERM",
    "ComSpec",
    "SystemRoot",
    "WINDIR",
];

#[derive(Clone, Debug)]
pub(crate) struct ExecutionEnvironmentSnapshot {
    os: OsDescriptor,
    container: ContainerDescriptor,
    workspace: WorkspaceDescriptor,
    commands: Vec<CommandDescriptor>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolGroupDisclosure {
    pub(crate) files: bool,
    pub(crate) web: bool,
    pub(crate) shell: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SemanticDescriptor<'a> {
    version: &'static str,
    scope: &'static str,
    os: &'a OsDescriptor,
    runtime: RuntimeDescriptor,
    execution: ExecutionDescriptor,
    container: &'a ContainerDescriptor,
    workspace: &'a WorkspaceDescriptor,
    tool_groups: ToolGroupDisclosure,
    commands: &'a [CommandDescriptor],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor<'a> {
    version: &'static str,
    snapshot_revision: String,
    scope: &'static str,
    os: &'a OsDescriptor,
    runtime: RuntimeDescriptor,
    execution: ExecutionDescriptor,
    container: &'a ContainerDescriptor,
    workspace: &'a WorkspaceDescriptor,
    tool_groups: ToolGroupDisclosure,
    commands: &'a [CommandDescriptor],
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OsDescriptor {
    family: &'static str,
    architecture: &'static str,
    path_style: &'static str,
    kernel_release: Option<String>,
    distribution: Option<String>,
    wsl: bool,
}

#[derive(Clone, Copy, Serialize)]
struct RuntimeDescriptor {
    name: &'static str,
    version: &'static str,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionDescriptor {
    shell: &'static str,
    sandbox: &'static str,
    network_access: &'static str,
    environment_inheritance: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct ContainerDescriptor {
    kind: &'static str,
    evidence: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
struct WorkspaceDescriptor {
    git: GitDescriptor,
    #[serde(rename = "packageManager")]
    package_manager: PackageManagerDescriptor,
}

#[derive(Clone, Debug, Serialize)]
struct GitDescriptor {
    available: bool,
    repository: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
struct PackageManagerDescriptor {
    #[serde(skip_serializing_if = "Option::is_none")]
    declared: Option<DeclaredPackageManager>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inferred: Option<String>,
    lockfiles: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
struct DeclaredPackageManager {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CommandDescriptor {
    id: &'static str,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Clone, Copy)]
struct Probe {
    id: &'static str,
    executable: &'static str,
    args: &'static [&'static str],
}

const PROBES: &[Probe] = &[
    probe("bash", "bash", &["--version"]),
    probe("zsh", "zsh", &["--version"]),
    probe("fish", "fish", &["--version"]),
    probe("cmd", "cmd", &["/C", "ver"]),
    probe("pwsh", "pwsh", &["--version"]),
    probe(
        "powershell",
        "powershell",
        &[
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "$PSVersionTable.PSVersion.ToString()",
        ],
    ),
    probe("python3", "python3", &["--version"]),
    probe("python", "python", &["--version"]),
    probe("uv", "uv", &["--version"]),
    probe("uvx", "uvx", &["--version"]),
    probe("node", "node", &["--version"]),
    probe("bun", "bun", &["--version"]),
    probe("pnpm", "pnpm", &["--version"]),
    probe("npm", "npm", &["--version"]),
    probe("yarn", "yarn", &["--version"]),
    probe("git", "git", &["--version"]),
    probe("rg", "rg", &["--version"]),
    probe("grep", "grep", &["--version"]),
    probe("docker", "docker", &["--version"]),
    probe("podman", "podman", &["--version"]),
    probe("nerdctl", "nerdctl", &["--version"]),
    probe("docker-compose", "docker-compose", &["--version"]),
    probe("kubectl", "kubectl", &["version", "--client"]),
    probe("devcontainer", "devcontainer", &["--version"]),
];

const LOCKFILES: &[(&str, &str)] = &[
    ("pnpm-lock.yaml", "pnpm"),
    ("package-lock.json", "npm"),
    ("npm-shrinkwrap.json", "npm"),
    ("yarn.lock", "yarn"),
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
];

const fn probe(id: &'static str, executable: &'static str, args: &'static [&'static str]) -> Probe {
    Probe {
        id,
        executable,
        args,
    }
}

impl ExecutionEnvironmentSnapshot {
    pub(crate) async fn collect(root: Option<&Path>) -> Self {
        let (kernel_release, distribution, container, mut workspace, commands) = tokio::join!(
            collect_kernel_release(),
            collect_distribution(),
            detect_container(),
            collect_workspace(root),
            collect_commands(),
        );
        workspace.git.available = commands
            .iter()
            .find(|command| command.id == "git")
            .is_some_and(|command| command.available);
        let wsl = container.kind == "wsl" || container.evidence.contains(&"wsl-kernel");
        Self {
            os: OsDescriptor {
                family: os_family(),
                architecture: architecture(),
                path_style: if cfg!(windows) { "windows" } else { "posix" },
                kernel_release,
                distribution,
                wsl,
            },
            container,
            workspace,
            commands,
        }
    }

    pub(crate) fn descriptor(
        &self,
        groups: ToolGroupDisclosure,
    ) -> serde_json::Map<String, serde_json::Value> {
        let execution = ExecutionDescriptor {
            shell: if groups.shell {
                process_shell()
            } else {
                "none"
            },
            sandbox: sandbox_for_container(self.container.kind),
            network_access: "host-policy",
            environment_inheritance: if groups.shell {
                "allowlisted"
            } else {
                "not-applicable"
            },
        };
        let runtime = RuntimeDescriptor {
            name: "workcell-mcp",
            version: env!("CARGO_PKG_VERSION"),
        };
        let semantic = SemanticDescriptor {
            version: "v1",
            scope: "server-process",
            os: &self.os,
            runtime,
            execution,
            container: &self.container,
            workspace: &self.workspace,
            tool_groups: groups,
            commands: &self.commands,
        };
        let revision = format!("sha256:{}", canonical_hash(&semantic));
        serde_json::to_value(Descriptor {
            version: "v1",
            snapshot_revision: revision,
            scope: "server-process",
            os: &self.os,
            runtime,
            execution,
            container: &self.container,
            workspace: &self.workspace,
            tool_groups: groups,
            commands: &self.commands,
        })
        .expect("static descriptor is serializable")
        .as_object()
        .expect("descriptor serializes as an object")
        .clone()
    }
}

fn canonical_hash(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).expect("static descriptor is serializable");
    let bytes = serde_json::to_vec(&canonicalize(value)).expect("canonical JSON is serializable");
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

async fn collect_commands() -> Vec<CommandDescriptor> {
    let mut tasks = JoinSet::new();
    for (index, probe) in PROBES.iter().copied().enumerate() {
        tasks.spawn(async move { (index, run_probe(probe).await) });
    }
    let mut commands = Vec::with_capacity(PROBES.len());
    while let Some(Ok(result)) = tasks.join_next().await {
        commands.push(result);
    }
    commands.sort_by_key(|(index, _)| *index);
    commands.into_iter().map(|(_, command)| command).collect()
}

async fn run_probe(probe: Probe) -> CommandDescriptor {
    let result = run_command(probe.executable, probe.args).await;
    CommandDescriptor {
        id: probe.id,
        available: result.spawned,
        version: result
            .success
            .then(|| extract_version(&result.stdout).or_else(|| extract_version(&result.stderr)))
            .flatten(),
    }
}

#[derive(Default)]
struct CommandResult {
    spawned: bool,
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_command(executable: &str, args: &[&str]) -> CommandResult {
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    inherit_allowlisted_environment(&mut command);
    let Ok(mut child) = command.spawn() else {
        return CommandResult::default();
    };
    let Some(stdout) = child.stdout.take() else {
        return CommandResult {
            spawned: true,
            ..CommandResult::default()
        };
    };
    let Some(stderr) = child.stderr.take() else {
        return CommandResult {
            spawned: true,
            ..CommandResult::default()
        };
    };
    tokio::time::timeout(PROBE_TIMEOUT, async {
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let mut bounded_stdout = stdout.take(MAX_PROBE_OUTPUT_BYTES);
        let mut bounded_stderr = stderr.take(MAX_PROBE_OUTPUT_BYTES);
        let (status, stdout_result, stderr_result) = tokio::join!(
            child.wait(),
            bounded_stdout.read_to_end(&mut stdout_bytes),
            bounded_stderr.read_to_end(&mut stderr_bytes),
        );
        CommandResult {
            spawned: true,
            success: status.is_ok_and(|status| status.success())
                && stdout_result.is_ok()
                && stderr_result.is_ok(),
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        }
    })
    .await
    .unwrap_or(CommandResult {
        spawned: true,
        ..CommandResult::default()
    })
}

fn inherit_allowlisted_environment(command: &mut Command) {
    for name in INHERITED_ENVIRONMENT {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

fn extract_version(output: &[u8]) -> Option<String> {
    let output = std::str::from_utf8(output).ok()?;
    output
        .split(|character: char| !is_version_character(character))
        .find_map(|token| {
            let token = token.trim_matches(['.', '+', '-', '_', '(', ')']);
            let token = token.strip_prefix('v').unwrap_or(token);
            is_normalized_version(token).then(|| token.to_owned())
        })
}

fn is_normalized_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-' | b'_' | b'(' | b')')
        })
}

fn is_version_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '+' | '-' | '_' | '(' | ')')
}

async fn collect_kernel_release() -> Option<String> {
    if cfg!(target_os = "linux") {
        return read_sanitized(Path::new("/proc/sys/kernel/osrelease"), 256).await;
    }
    if cfg!(target_os = "macos") {
        let result = run_command("uname", &["-r"]).await;
        if result.success {
            return sanitize_system_string(std::str::from_utf8(&result.stdout).ok()?);
        }
    }
    None
}

async fn collect_distribution() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let bytes = read_bounded(Path::new("/etc/os-release"), MAX_METADATA_BYTES).await?;
    let contents = std::str::from_utf8(&bytes).ok()?;
    let id = os_release_value(contents, "ID").and_then(sanitize_system_string)?;
    let version = os_release_value(contents, "VERSION_ID").and_then(sanitize_system_string);
    version
        .and_then(|version| sanitize_system_string(&format!("{id} {version}")))
        .or(Some(id))
}

fn os_release_value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|value| value.strip_prefix('='))
            .map(|value| value.trim_matches(['\'', '"']))
    })
}

async fn read_sanitized(path: &Path, limit: u64) -> Option<String> {
    let bytes = read_bounded(path, limit).await?;
    sanitize_system_string(std::str::from_utf8(&bytes).ok()?)
}

fn sanitize_system_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'+' | b'-' | b'_' | b'(' | b')' | b' ')
        }))
    .then(|| value.to_owned())
}

async fn read_bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).await.ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

async fn detect_container() -> ContainerDescriptor {
    let mut evidence = Vec::new();
    let mut kind = "none";

    if std::env::var_os("container").is_some() {
        push_evidence(&mut evidence, "env-container");
        kind = "container";
    }
    if std::env::var_os("REMOTE_CONTAINERS").is_some() || std::env::var_os("DEVCONTAINER").is_some()
    {
        push_evidence(&mut evidence, "env-devcontainer");
        kind = "devcontainer";
    }
    if std::env::var_os("CODESPACES").is_some() {
        push_evidence(&mut evidence, "env-codespaces");
        kind = "codespaces";
    }
    if tokio::fs::metadata("/.dockerenv").await.is_ok() {
        push_evidence(&mut evidence, "dockerenv");
        if kind == "none" || kind == "container" {
            kind = "docker";
        }
    }
    if tokio::fs::metadata("/run/.containerenv").await.is_ok() {
        push_evidence(&mut evidence, "containerenv");
        if kind == "none" || kind == "container" {
            kind = "podman";
        }
    }
    if let Some(cgroup) = read_bounded(Path::new("/proc/1/cgroup"), MAX_METADATA_BYTES).await
        && let Ok(cgroup) = std::str::from_utf8(&cgroup)
        && let Some(detected) = classify_cgroup(cgroup)
    {
        push_evidence(&mut evidence, "proc-cgroup");
        if !matches!(kind, "codespaces" | "devcontainer") {
            kind = detected;
        }
    }
    let kernel = read_bounded(Path::new("/proc/sys/kernel/osrelease"), 256).await;
    if kernel.as_deref().is_some_and(|value| {
        String::from_utf8_lossy(value)
            .to_ascii_lowercase()
            .contains("microsoft")
    }) {
        push_evidence(&mut evidence, "wsl-kernel");
        kind = "wsl";
    }

    let detected = run_command("systemd-detect-virt", &[]).await;
    if detected.success
        && let Some(detected_kind) = classify_virtualization_output(&detected.stdout)
    {
        push_evidence(&mut evidence, "systemd-detect-virt");
        if kind == "none" || kind == "container" {
            kind = detected_kind;
        }
    }

    ContainerDescriptor { kind, evidence }
}

fn push_evidence(evidence: &mut Vec<&'static str>, value: &'static str) {
    if !evidence.contains(&value) {
        evidence.push(value);
    }
}

fn classify_cgroup(value: &str) -> Option<&'static str> {
    let value = value.to_ascii_lowercase();
    if value.contains("kubepods") {
        Some("kubernetes")
    } else if value.contains("docker") {
        Some("docker")
    } else if value.contains("containerd") {
        Some("containerd")
    } else if value.contains("podman") || value.contains("libpod") {
        Some("podman")
    } else if value.contains("lxc") {
        Some("lxc")
    } else {
        None
    }
}

fn classify_virtualization_output(output: &[u8]) -> Option<&'static str> {
    match std::str::from_utf8(output)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "docker" => Some("docker"),
        "podman" => Some("podman"),
        "containerd" => Some("containerd"),
        "lxc" | "lxc-libvirt" => Some("lxc"),
        "wsl" => Some("wsl"),
        "kvm" | "qemu" | "vmware" | "microsoft" | "oracle" | "xen" | "bochs" | "uml"
        | "parallels" | "bhyve" => Some("virtual-machine"),
        "none" => Some("none"),
        _ => Some("unknown"),
    }
}

fn sandbox_for_container(kind: &str) -> &'static str {
    match kind {
        "docker" | "podman" | "containerd" | "kubernetes" | "lxc" | "devcontainer"
        | "codespaces" | "wsl" | "container" => "container",
        "virtual-machine" => "virtual-machine",
        _ => "unknown",
    }
}

async fn collect_workspace(root: Option<&Path>) -> WorkspaceDescriptor {
    let Some(root) = root else {
        return WorkspaceDescriptor {
            git: GitDescriptor {
                available: false,
                repository: "unknown",
            },
            package_manager: PackageManagerDescriptor::default(),
        };
    };

    let git_result = run_git_repository_probe(root).await;
    let package_manager = collect_package_manager(root).await;
    WorkspaceDescriptor {
        git: GitDescriptor {
            available: git_result.spawned,
            repository: if !git_result.spawned {
                "unknown"
            } else if git_result.success && trim_ascii(&git_result.stdout) == b"true" {
                "yes"
            } else {
                "no"
            },
        },
        package_manager,
    }
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &value[start..end]
}

async fn run_git_repository_probe(root: &Path) -> CommandResult {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    inherit_allowlisted_environment(&mut command);
    let Ok(mut child) = command.spawn() else {
        return CommandResult::default();
    };
    let Some(stdout) = child.stdout.take() else {
        return CommandResult {
            spawned: true,
            ..CommandResult::default()
        };
    };
    tokio::time::timeout(PROBE_TIMEOUT, async {
        let mut stdout_bytes = Vec::new();
        let mut bounded_stdout = stdout.take(16);
        let (status, output) =
            tokio::join!(child.wait(), bounded_stdout.read_to_end(&mut stdout_bytes));
        CommandResult {
            spawned: true,
            success: status.is_ok_and(|status| status.success()) && output.is_ok(),
            stdout: stdout_bytes,
            stderr: Vec::new(),
        }
    })
    .await
    .unwrap_or(CommandResult {
        spawned: true,
        ..CommandResult::default()
    })
}

async fn collect_package_manager(root: &Path) -> PackageManagerDescriptor {
    let declared = read_bounded(&root.join("package.json"), MAX_METADATA_BYTES)
        .await
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|package| {
            package
                .get("packageManager")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_declared_package_manager)
        });
    let mut lockfiles = Vec::new();
    let mut inferred = declared.as_ref().map(|manager| manager.name.clone());
    for &(filename, manager) in LOCKFILES {
        if tokio::fs::metadata(root.join(filename)).await.is_ok() {
            lockfiles.push(filename);
            inferred.get_or_insert_with(|| manager.to_owned());
        }
    }
    PackageManagerDescriptor {
        declared,
        inferred,
        lockfiles,
    }
}

fn parse_declared_package_manager(value: &str) -> Option<DeclaredPackageManager> {
    let split = value.rfind('@').filter(|index| *index > 0);
    let (name, version) = split.map_or((value, None), |index| {
        (&value[..index], value.get(index + 1..))
    });
    if !is_package_manager_name(name) {
        return None;
    }
    let version = version.filter(|version| is_normalized_version(version));
    Some(DeclaredPackageManager {
        name: name.to_owned(),
        version: version.map(ToOwned::to_owned),
    })
}

fn is_package_manager_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.contains("..")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'/' | b'.' | b'-' | b'_')
        })
}

const fn process_shell() -> &'static str {
    if cfg!(windows) {
        "cmd"
    } else if cfg!(unix) {
        "bash"
    } else {
        "other"
    }
}

const fn os_family() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "other"
    }
}

const fn architecture() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn version_normalization_never_returns_raw_output_or_paths() {
        assert_eq!(
            extract_version(b"git version 2.45.1\n"),
            Some("2.45.1".into())
        );
        assert_eq!(extract_version(b"/secret/canary/tool\n"), None);
        assert_eq!(extract_version(b"secret-canary\n"), None);
        assert_eq!(extract_version(b"/tmp/secret-canary-9\n"), None);
        assert_eq!(extract_version(&[b'1'; 65]), None);
    }

    #[test]
    fn package_manager_parser_rejects_unsanitized_metadata() {
        assert_eq!(
            serde_json::to_value(parse_declared_package_manager("pnpm@9.15.0").unwrap()).unwrap(),
            serde_json::json!({"name":"pnpm","version":"9.15.0"})
        );
        assert!(
            parse_declared_package_manager("pnpm@/secret/canary")
                .is_some_and(|value| value.version.is_none())
        );
        assert!(parse_declared_package_manager("../../secret@1.0.0").is_none());
    }

    #[tokio::test]
    async fn workspace_metadata_contains_only_sanitized_package_metadata() {
        let root = tempdir().unwrap();
        tokio::fs::write(
            root.path().join("package.json"),
            br#"{"packageManager":"pnpm@9.15.0","name":"sensitive-project","author":"secret"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(root.path().join("pnpm-lock.yaml"), b"secret: canary")
            .await
            .unwrap();
        let workspace = collect_workspace(Some(root.path())).await;
        let value = serde_json::to_value(workspace).unwrap();
        assert_eq!(
            value["packageManager"],
            serde_json::json!({
                "declared":{"name":"pnpm","version":"9.15.0"},
                "inferred":"pnpm",
                "lockfiles":["pnpm-lock.yaml"]
            })
        );
        assert!(!value.to_string().contains("sensitive-project"));
        assert!(!value.to_string().contains("canary"));
        assert!(
            !value
                .to_string()
                .contains(root.path().to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn descriptor_is_strict_and_revision_is_stable() {
        let snapshot = ExecutionEnvironmentSnapshot::collect(None).await;
        let groups = ToolGroupDisclosure {
            files: true,
            web: false,
            shell: false,
        };
        let first = snapshot.descriptor(groups);
        let second = snapshot.descriptor(groups);
        assert_eq!(first, second);
        assert_eq!(first.len(), 10);
        assert_eq!(
            first.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "version",
                "snapshotRevision",
                "scope",
                "os",
                "runtime",
                "execution",
                "container",
                "workspace",
                "toolGroups",
                "commands",
            ]
        );
        assert_eq!(first["version"], "v1");
        assert_eq!(first["scope"], "server-process");
        assert_eq!(first["runtime"]["name"], "workcell-mcp");
        assert_eq!(first["runtime"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(first["os"].as_object().unwrap().len(), 6);
        assert!(matches!(
            first["os"]["family"].as_str(),
            Some("linux" | "macos" | "windows" | "other")
        ));
        assert!(matches!(
            first["os"]["architecture"].as_str(),
            Some("x86_64" | "aarch64" | "x86" | "arm" | "other")
        ));
        assert!(first["os"]["wsl"].is_boolean());
        assert!(matches!(
            first["container"]["kind"].as_str(),
            Some(
                "none"
                    | "docker"
                    | "podman"
                    | "containerd"
                    | "kubernetes"
                    | "lxc"
                    | "devcontainer"
                    | "codespaces"
                    | "wsl"
                    | "container"
                    | "virtual-machine"
                    | "unknown"
            )
        ));
        assert!(
            first["container"]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|evidence| matches!(
                    evidence.as_str(),
                    Some(
                        "env-container"
                            | "env-codespaces"
                            | "env-devcontainer"
                            | "dockerenv"
                            | "containerenv"
                            | "proc-cgroup"
                            | "systemd-detect-virt"
                            | "wsl-kernel"
                    )
                ))
        );
        assert_eq!(first["execution"]["shell"], "none");
        assert_eq!(
            first["execution"]["environmentInheritance"],
            "not-applicable"
        );
        assert_eq!(first["workspace"]["git"]["repository"], "unknown");
        assert_eq!(
            first["workspace"]["packageManager"],
            serde_json::json!({"lockfiles":[]})
        );
        assert_eq!(first["commands"].as_array().unwrap().len(), PROBES.len());
        assert_eq!(
            first["commands"]
                .as_array()
                .unwrap()
                .iter()
                .map(|command| command["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            PROBES.iter().map(|probe| probe.id).collect::<Vec<_>>()
        );
        assert!(first["commands"].as_array().unwrap().iter().all(|command| {
            command.as_object().is_some_and(|command| {
                command
                    .keys()
                    .all(|key| matches!(key.as_str(), "id" | "available" | "version"))
                    && command["available"].is_boolean()
            })
        }));
        let revision = first["snapshotRevision"].as_str().unwrap();
        assert_eq!(revision.len(), 71);
        assert!(
            revision
                .strip_prefix("sha256:")
                .unwrap()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }
}
