use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use rmcp::model::{CallToolResult, ContentBlock, JsonObject, MetaObject, Tool, ToolAnnotations};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub(crate) const EXTENSION_ID: &str = "ai.workcell/execution-environment";
pub(crate) const TOOL_NAME: &str = "execution_environment";

const PRESENTATION_KEY: &str = "ai.workcell/presentation-profile";
const TOOL_DESCRIPTION: &str = r#"Inspect the MCP server's current sanitized execution environment.

Use this tool when command availability, installed versions, Git repository status, package-manager metadata, or recognized lockfiles may have changed since server discovery. Each call collects a fresh snapshot using bounded local checks; avoid repeated calls when an earlier result is still sufficient.

The result reports platform and runtime classifications, enabled tool groups, workspace metadata, and availability and normalized versions for a fixed list of commands. `available` means the fixed executable resolved outside the configured root and could be started, not that every operation it supports is permitted or safe. Version fields are included only when bounded output contains a valid normalized version.

The tool accepts no arguments and never runs client-provided commands. It may start fixed local executables with version or client-only arguments from a working directory outside the configured root; accepted executables may invoke their own subprocesses from the same root-filtered `PATH`. It omits raw paths, environment values, probe output, file contents, tool arguments, and credentials. Container, sandbox, network, and command classifications are best-effort observations; do not use them as security guarantees."#;

const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const INSPECTION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PROBE_OUTPUT_BYTES: u64 = 4_096;
const MAX_METADATA_BYTES: u64 = 64 * 1024;
const INHERITED_ENVIRONMENT: [&str; 6] = ["PATH", "LANG", "LC_ALL", "TERM", "SystemRoot", "WINDIR"];

#[derive(Clone, Debug)]
pub(crate) struct ExecutionEnvironmentSnapshot {
    os: OsDescriptor,
    container: ContainerDescriptor,
    workspace: WorkspaceDescriptor,
    commands: Vec<CommandDescriptor>,
}

#[derive(Clone, Debug)]
pub(crate) struct ExecutionEnvironmentDisclosure {
    root: Option<PathBuf>,
    startup: ExecutionEnvironmentSnapshot,
    refresh_gate: Arc<Semaphore>,
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

impl ExecutionEnvironmentDisclosure {
    pub(crate) async fn collect(root: Option<&Path>) -> Self {
        let root = canonical_root(root).await;
        Self {
            startup: ExecutionEnvironmentSnapshot::collect(root.as_deref()).await,
            root,
            refresh_gate: Arc::new(Semaphore::new(1)),
        }
    }

    pub(crate) fn discovery_descriptor(
        &self,
        groups: ToolGroupDisclosure,
    ) -> serde_json::Map<String, Value> {
        self.startup.descriptor(groups)
    }

    pub(crate) async fn call_tool(
        &self,
        arguments: Value,
        groups: ToolGroupDisclosure,
        cancellation: CancellationToken,
    ) -> CallToolResult {
        if !matches!(&arguments, Value::Object(values) if values.is_empty()) {
            return tool_error(
                "Invalid arguments for tool execution_environment: expected an empty object",
            );
        }

        let gate = self.refresh_gate.clone();
        let permit = tokio::select! {
            permit = gate.acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return tool_error("Execution-environment refresh gate is unavailable"),
            },
            () = cancellation.cancelled() => {
                return tool_error("Execution-environment inspection cancelled");
            }
        };
        let collection = tokio::time::timeout(
            INSPECTION_TIMEOUT,
            ExecutionEnvironmentSnapshot::collect(self.root.as_deref()),
        );
        tokio::pin!(collection);
        let snapshot = tokio::select! {
            snapshot = &mut collection => match snapshot {
                Ok(snapshot) => snapshot,
                Err(_) => return tool_error("Execution-environment inspection timed out"),
            },
            () = cancellation.cancelled() => {
                // The total collection deadline bounds cleanup. Let it finish before releasing the
                // gate so cancellation cannot create overlapping process batches.
                let _ = collection.await;
                return tool_error("Execution-environment inspection cancelled");
            }
        };
        drop(permit);
        success(snapshot.descriptor(groups))
    }
}

pub(crate) fn tool() -> Tool {
    let input_schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {},
        "additionalProperties": false
    });
    let mut meta = JsonObject::new();
    meta.insert(
        PRESENTATION_KEY.to_owned(),
        Value::String("execution-environment.snapshot.v1".to_owned()),
    );
    Tool::new(
        TOOL_NAME,
        TOOL_DESCRIPTION,
        Arc::new(
            input_schema
                .as_object()
                .expect("execution-environment input schema is an object")
                .clone(),
        ),
    )
    .with_title("Inspect execution environment")
    .with_raw_output_schema(output_schema())
    .with_annotations(ToolAnnotations::from_raw(
        None,
        Some(true),
        Some(false),
        Some(true),
        Some(false),
    ))
    .with_meta(MetaObject(meta))
}

fn output_schema() -> Arc<JsonObject> {
    let nullable_string = json!({"type": ["string", "null"], "maxLength": 128});
    let lockfiles = LOCKFILES
        .iter()
        .map(|(filename, _)| *filename)
        .collect::<Vec<_>>();
    let command_ids = PROBES.iter().map(|probe| probe.id).collect::<Vec<_>>();
    let command_count = PROBES.len();
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "version", "snapshotRevision", "scope", "os", "runtime", "execution",
            "container", "workspace", "toolGroups", "commands"
        ],
        "properties": {
            "version": {"const": "v1"},
            "snapshotRevision": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$"},
            "scope": {"const": "server-process"},
            "os": {
                "type": "object",
                "additionalProperties": false,
                "required": ["family", "architecture", "pathStyle", "kernelRelease", "distribution", "wsl"],
                "properties": {
                    "family": {"enum": ["linux", "macos", "windows", "other"]},
                    "architecture": {"enum": ["x86_64", "aarch64", "x86", "arm", "other"]},
                    "pathStyle": {"enum": ["windows", "posix"]},
                    "kernelRelease": nullable_string,
                    "distribution": {"type": ["string", "null"], "maxLength": 128},
                    "wsl": {"type": "boolean"}
                }
            },
            "runtime": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "version"],
                "properties": {
                    "name": {"const": "workcell-mcp"},
                    "version": {"type": "string"}
                }
            },
            "execution": {
                "type": "object",
                "additionalProperties": false,
                "required": ["shell", "sandbox", "networkAccess", "environmentInheritance"],
                "properties": {
                    "shell": {"enum": ["bash", "cmd", "other", "none"]},
                    "sandbox": {"enum": ["container", "virtual-machine", "unknown"]},
                    "networkAccess": {"const": "host-policy"},
                    "environmentInheritance": {"enum": ["allowlisted", "not-applicable"]}
                }
            },
            "container": {
                "type": "object",
                "additionalProperties": false,
                "required": ["kind", "evidence"],
                "properties": {
                    "kind": {"enum": [
                        "none", "docker", "podman", "containerd", "kubernetes", "lxc",
                        "devcontainer", "codespaces", "wsl", "container", "virtual-machine", "unknown"
                    ]},
                    "evidence": {
                        "type": "array",
                        "uniqueItems": true,
                        "items": {"enum": [
                            "env-container", "env-codespaces", "env-devcontainer", "dockerenv",
                            "containerenv", "proc-cgroup", "systemd-detect-virt", "wsl-kernel"
                        ]}
                    }
                }
            },
            "workspace": {
                "type": "object",
                "additionalProperties": false,
                "required": ["git", "packageManager"],
                "properties": {
                    "git": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["available", "repository"],
                        "properties": {
                            "available": {"type": "boolean"},
                            "repository": {"enum": ["yes", "no", "unknown"]}
                        }
                    },
                    "packageManager": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["lockfiles"],
                        "properties": {
                            "declared": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["name"],
                                "properties": {
                                    "name": {"type": "string", "minLength": 1, "maxLength": 64},
                                    "version": {"type": "string", "minLength": 1, "maxLength": 64}
                                }
                            },
                            "inferred": {"type": "string", "minLength": 1, "maxLength": 64},
                            "lockfiles": {
                                "type": "array",
                                "uniqueItems": true,
                                "items": {"enum": lockfiles}
                            }
                        }
                    }
                }
            },
            "toolGroups": {
                "type": "object",
                "additionalProperties": false,
                "required": ["files", "web", "shell"],
                "properties": {
                    "files": {"type": "boolean"},
                    "web": {"type": "boolean"},
                    "shell": {"type": "boolean"}
                }
            },
            "commands": {
                "type": "array",
                "minItems": command_count,
                "maxItems": command_count,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "available"],
                    "properties": {
                        "id": {"enum": command_ids},
                        "available": {"type": "boolean"},
                        "version": {"type": "string", "minLength": 1, "maxLength": 64}
                    }
                }
            }
        }
    });
    Arc::new(
        schema
            .as_object()
            .expect("execution-environment output schema is an object")
            .clone(),
    )
}

fn success(descriptor: serde_json::Map<String, Value>) -> CallToolResult {
    let structured = Value::Object(descriptor);
    let text = serde_json::to_string_pretty(&structured)
        .expect("execution-environment descriptor is serializable");
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(structured);
    result
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

impl ExecutionEnvironmentSnapshot {
    pub(crate) async fn collect(root: Option<&Path>) -> Self {
        let canonical_root = canonical_root(root).await;
        let root = canonical_root.as_deref();
        let (kernel_release, distribution, container, mut workspace, commands) = tokio::join!(
            collect_kernel_release(root),
            collect_distribution(),
            detect_container(root),
            collect_workspace(root),
            collect_commands(root),
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

async fn canonical_root(root: Option<&Path>) -> Option<PathBuf> {
    let root = root?;
    Some(
        tokio::fs::canonicalize(root)
            .await
            .unwrap_or_else(|_| root.to_path_buf()),
    )
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

async fn collect_commands(excluded_root: Option<&Path>) -> Vec<CommandDescriptor> {
    let mut tasks = JoinSet::new();
    for (index, probe) in PROBES.iter().copied().enumerate() {
        let excluded_root = excluded_root.map(Path::to_path_buf);
        tasks.spawn(async move { (index, run_probe(probe, excluded_root.as_deref()).await) });
    }
    let mut commands = Vec::with_capacity(PROBES.len());
    while let Some(Ok(result)) = tasks.join_next().await {
        commands.push(result);
    }
    commands.sort_by_key(|(index, _)| *index);
    commands.into_iter().map(|(_, command)| command).collect()
}

async fn run_probe(probe: Probe, excluded_root: Option<&Path>) -> CommandDescriptor {
    let result = run_command(probe.executable, probe.args, excluded_root).await;
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

async fn run_command(
    executable: &str,
    args: &[&str],
    excluded_root: Option<&Path>,
) -> CommandResult {
    let Some(executable) = resolve_executable(executable, excluded_root).await else {
        return CommandResult::default();
    };
    let working_directory = executable.path.parent().map(Path::to_path_buf);
    let mut command = Command::new(executable.path);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(working_directory) = working_directory {
        command.current_dir(working_directory);
    }
    configure_probe_process(&mut command);
    inherit_allowlisted_environment(&mut command, &executable.search_path);
    let Ok(mut child) = command.spawn() else {
        return CommandResult::default();
    };
    let pid = child.id();
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
    match tokio::time::timeout(PROBE_TIMEOUT, async {
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
    {
        Ok(result) => result,
        Err(_) => {
            terminate_probe(&mut child, pid).await;
            CommandResult {
                spawned: true,
                ..CommandResult::default()
            }
        }
    }
}

struct ResolvedExecutable {
    path: PathBuf,
    search_path: std::ffi::OsString,
}

async fn resolve_executable(
    name: &str,
    excluded_root: Option<&Path>,
) -> Option<ResolvedExecutable> {
    let inherited_path = std::env::var_os("PATH")?;
    let search_path = filtered_search_path(&inherited_path, excluded_root).await?;
    let path = resolve_executable_in(name, &search_path, excluded_root).await?;
    Some(ResolvedExecutable { path, search_path })
}

async fn filtered_search_path(
    search_path: &std::ffi::OsStr,
    excluded_root: Option<&Path>,
) -> Option<std::ffi::OsString> {
    let mut safe_directories = Vec::new();
    for directory in std::env::split_paths(search_path) {
        let Ok(canonical) = tokio::fs::canonicalize(directory).await else {
            continue;
        };
        if excluded_root.is_some_and(|root| canonical.starts_with(root)) {
            continue;
        }
        if tokio::fs::metadata(&canonical)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            safe_directories.push(canonical);
        }
    }
    (!safe_directories.is_empty())
        .then(|| std::env::join_paths(safe_directories).ok())
        .flatten()
}

async fn resolve_executable_in(
    name: &str,
    search_path: &std::ffi::OsStr,
    excluded_root: Option<&Path>,
) -> Option<PathBuf> {
    for directory in std::env::split_paths(search_path) {
        for candidate in executable_candidates(&directory, name) {
            let Ok(canonical) = tokio::fs::canonicalize(candidate).await else {
                continue;
            };
            if excluded_root.is_some_and(|root| canonical.starts_with(root)) {
                continue;
            }
            let Ok(metadata) = tokio::fs::metadata(&canonical).await else {
                continue;
            };
            if is_executable_file(&metadata, &canonical) {
                return Some(canonical);
            }
        }
    }
    None
}

fn executable_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    let candidate = directory.join(name);
    if !cfg!(windows) || candidate.extension().is_some() {
        return vec![candidate];
    }
    ["", ".com", ".exe", ".bat", ".cmd"]
        .into_iter()
        .map(|extension| directory.join(format!("{name}{extension}")))
        .collect()
}

fn is_executable_file(metadata: &std::fs::Metadata, _path: &Path) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        _path.extension().is_some_and(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "com" | "exe" | "bat" | "cmd"
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = _path;
        true
    }
}

fn configure_probe_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

async fn terminate_probe(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        if let Some(pid) = pid
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
        {
            let _ = kill_process_group(pid, Signal::Kill);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn inherit_allowlisted_environment(command: &mut Command, search_path: &std::ffi::OsStr) {
    for name in INHERITED_ENVIRONMENT {
        if name == "PATH" {
            command.env(name, search_path);
            continue;
        }
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

async fn collect_kernel_release(excluded_root: Option<&Path>) -> Option<String> {
    if cfg!(target_os = "linux") {
        return read_sanitized(Path::new("/proc/sys/kernel/osrelease"), 256).await;
    }
    if cfg!(target_os = "macos") {
        let result = run_command("uname", &["-r"], excluded_root).await;
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

async fn detect_container(excluded_root: Option<&Path>) -> ContainerDescriptor {
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

    let detected = run_command("systemd-detect-virt", &[], excluded_root).await;
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
    let Some(git) = resolve_executable("git", Some(root)).await else {
        return CommandResult::default();
    };
    let working_directory = git.path.parent().map(Path::to_path_buf);
    let mut command = Command::new(git.path);
    command
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(working_directory) = working_directory {
        command.current_dir(working_directory);
    }
    configure_probe_process(&mut command);
    inherit_allowlisted_environment(&mut command, &git.search_path);
    let Ok(mut child) = command.spawn() else {
        return CommandResult::default();
    };
    let pid = child.id();
    let Some(stdout) = child.stdout.take() else {
        return CommandResult {
            spawned: true,
            ..CommandResult::default()
        };
    };
    match tokio::time::timeout(PROBE_TIMEOUT, async {
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
    {
        Ok(result) => result,
        Err(_) => {
            terminate_probe(&mut child, pid).await;
            CommandResult {
                spawned: true,
                ..CommandResult::default()
            }
        }
    }
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
    fn tool_catalog_matches_conformance_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../fixtures/mcp-conformance/catalog/v1/execution-environment-tool.json"
        ))
        .expect("execution-environment catalog fixture");
        assert_eq!(
            serde_json::to_value(tool()).unwrap(),
            fixture["expected"]["tools"][0]
        );
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn executable_resolution_observes_installs_but_rejects_workspace_targets() {
        use std::os::unix::fs::PermissionsExt;

        let bin = tempdir().unwrap();
        let root = tempdir().unwrap();
        let search_path = std::env::join_paths([bin.path()]).unwrap();
        assert!(
            resolve_executable_in("workcell-probe", &search_path, Some(root.path()))
                .await
                .is_none()
        );

        let executable = bin.path().join("workcell-probe");
        tokio::fs::write(&executable, b"#!/bin/sh\nprintf '1.0.0\\n'\n")
            .await
            .unwrap();
        let mut permissions = tokio::fs::metadata(&executable)
            .await
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(&executable, permissions)
            .await
            .unwrap();

        assert_eq!(
            resolve_executable_in("workcell-probe", &search_path, Some(root.path())).await,
            Some(tokio::fs::canonicalize(&executable).await.unwrap())
        );
        assert!(
            resolve_executable_in("workcell-probe", &search_path, Some(bin.path()))
                .await
                .is_none()
        );
        assert!(
            filtered_search_path(&search_path, Some(bin.path()))
                .await
                .is_none()
        );
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
    async fn tool_collects_fresh_workspace_state_and_changes_revision() {
        let root = tempdir().unwrap();
        let disclosure = ExecutionEnvironmentDisclosure::collect(Some(root.path())).await;
        let groups = ToolGroupDisclosure {
            files: true,
            web: false,
            shell: true,
        };
        let first = disclosure
            .call_tool(json!({}), groups, CancellationToken::new())
            .await;
        let first = first.structured_content.unwrap();
        assert_eq!(
            first["workspace"]["packageManager"],
            json!({"lockfiles": []})
        );

        tokio::fs::write(
            root.path().join("package.json"),
            br#"{"packageManager":"pnpm@10.1.0","name":"must-not-leak"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(root.path().join("pnpm-lock.yaml"), b"must-not-leak")
            .await
            .unwrap();

        let second = disclosure
            .call_tool(json!({}), groups, CancellationToken::new())
            .await;
        let ContentBlock::Text(text) = &second.content[0] else {
            panic!("expected text content");
        };
        let second = second.structured_content.unwrap();
        assert_ne!(first["snapshotRevision"], second["snapshotRevision"]);
        assert_eq!(
            second["workspace"]["packageManager"],
            json!({
                "declared": {"name": "pnpm", "version": "10.1.0"},
                "inferred": "pnpm",
                "lockfiles": ["pnpm-lock.yaml"]
            })
        );
        assert_eq!(text.text, serde_json::to_string_pretty(&second).unwrap());
        assert!(!text.text.contains("must-not-leak"));
        assert!(!text.text.contains(root.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn tool_rejects_arguments_and_honors_cancellation_while_queued() {
        let disclosure = ExecutionEnvironmentDisclosure::collect(None).await;
        let invalid = disclosure
            .call_tool(
                json!({"refresh": true}),
                ToolGroupDisclosure::default(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(invalid.is_error, Some(true));

        let _permit = disclosure.refresh_gate.acquire().await.unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = disclosure
            .call_tool(json!({}), ToolGroupDisclosure::default(), cancellation)
            .await;
        assert_eq!(cancelled.is_error, Some(true));
        let ContentBlock::Text(text) = &cancelled.content[0] else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "Execution-environment inspection cancelled");
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
