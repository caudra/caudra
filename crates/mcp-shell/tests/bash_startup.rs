#![cfg(unix)]

use std::{
    env, fs, iter,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::{process::Command, time::timeout};
use tokio_util::sync::CancellationToken;
use workcell_mcp_shell::bash::{BashContextError, BashCwdSet};
use workcell_mcp_shell::{ShellInput, ShellPermissionPolicy, ShellToolGroup};

const FIXTURE_ROOT: &str = "WORKCELL_BASH_STARTUP_FIXTURE";
const FIXTURE_MODE: &str = "WORKCELL_BASH_LAUNCHER_FIXTURE_MODE";
const BASH_EXECUTABLE_ENV: &str = "WORKCELL_BASH_EXECUTABLE";
const DEFAULT_MODE: &str = "default";
const RELATIVE_MODE: &str = "relative";
const MISSING_MODE: &str = "missing";
const PINNED_MODE: &str = "pinned";
const REPLACED_MODE: &str = "replaced";
const RETARGETED_MODE: &str = "retargeted";
const LAUNCHER_MODES: &[&str] = &[
    DEFAULT_MODE,
    RELATIVE_MODE,
    MISSING_MODE,
    PINNED_MODE,
    REPLACED_MODE,
    RETARGETED_MODE,
];
const FAKE_BASH_MARKER: &str = "fake-bash-ran";
const FAKE_BASH_SOURCE: &str =
    "#!/bin/sh\nprintf hijacked > \"$HOME/fake-bash-ran\"\ncd \"$HOME\"\nexit 75\n";
const EXECUTABLE_MODE: u32 = 0o700;
const NOTE: &str = "hidden-cdpath-canary\n";
const CHILD_TEST: &str = "isolated_startup_environment_child";
const CHILD_PASSED: &str = "workcell-startup-isolation-passed";
const CHILD_DEADLINE: Duration = Duration::from_secs(30);
const SHELL_TIMEOUT_MS: u64 = 5000;
const PROFILE_FILES: &[&str] = &[
    ".profile",
    ".bash_profile",
    ".bash_login",
    ".bashrc",
    "bash_env",
];
const STARTUP_MARKER: &str = "startup-ran";
const STARTUP_SOURCE: &str = "printf started > \"$HOME/startup-ran\"\ntrap 'printf startup-trap' DEBUG\ncd \"$HOME\"\nexit 73\n";
const PROXY_CANARY: &str = "workcell-proxy-canary";
const FUNCTION_SOURCE: &str = "() { builtin cd \"$HOME\"; printf inherited-function; }";
const PROBE: &str = r#"
if shopt -q login_shell || shopt -q lastpipe || shopt -q expand_aliases || shopt -q extdebug; then exit 71; fi
if [[ -o posix || -o noclobber || -o errexit || -o functrace || -o xtrace ]]; then exit 71; fi
if [[ -n $(declare -F) || -n $(trap -p) || $(type -t cd) != builtin ]]; then exit 71; fi
printf '%s\n' "$BASH" "$PWD" "$HOME" "$PATH" "$HTTPS_PROXY" "${BASH_ENV-unset}" "${ENV-unset}" "${CDPATH-unset}" "${PROMPT_COMMAND-unset}" "${WORKCELL_BASH_STARTUP_FIXTURE-unset}" "${WORKCELL_BASH_EXECUTABLE-unset}"
"#;

#[tokio::test]
async fn real_shell_startup_ignores_profiles_hooks_functions_and_inherited_options() {
    for mode in LAUNCHER_MODES {
        run_startup_fixture(mode).await;
    }
}

async fn run_startup_fixture(mode: &str) {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::create_dir(directory.path().join("work")).unwrap();
    let prefix = directory.path().join("path-canary");
    fs::create_dir(&prefix).unwrap();
    let fake = prefix.join("bash");
    fs::write(&fake, FAKE_BASH_SOURCE).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(EXECUTABLE_MODE)).unwrap();
    for name in PROFILE_FILES {
        fs::write(home.join(name), STARTUP_SOURCE).unwrap();
    }
    let inherited_path = env::var_os("PATH").unwrap();
    let path = env::join_paths(
        iter::once(directory.path().join("path-canary")).chain(env::split_paths(&inherited_path)),
    )
    .unwrap();
    let mut child = Command::new(env::current_exe().unwrap());
    match mode {
        RELATIVE_MODE => {
            child.env(BASH_EXECUTABLE_ENV, "bash");
        }
        MISSING_MODE => {
            child.env(BASH_EXECUTABLE_ENV, directory.path().join("missing-bash"));
        }
        PINNED_MODE | REPLACED_MODE | RETARGETED_MODE => {
            let group =
                ShellToolGroup::with_policy(directory.path(), ShellPermissionPolicy::yolo())
                    .await
                    .unwrap();
            let prepared = group.prepare(input("pwd")).await.unwrap();
            let selected = directory.path().join("selected-bash");
            if mode == RETARGETED_MODE {
                symlink(prepared.bash_executable().unwrap(), &selected).unwrap();
            } else {
                fs::copy(prepared.bash_executable().unwrap(), &selected).unwrap();
            }
            child.env(BASH_EXECUTABLE_ENV, selected);
        }
        DEFAULT_MODE => {}
        _ => panic!("unexpected fixture mode"),
    }
    child
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(FIXTURE_ROOT, directory.path())
        .env(FIXTURE_MODE, mode)
        .env("HOME", &home)
        .env("PATH", path)
        .env("HTTPS_PROXY", PROXY_CANARY)
        .env("BASH_ENV", home.join("bash_env"))
        .env("ENV", home.join("bash_env"))
        .env("SHELLOPTS", "errexit:functrace:noclobber:posix:xtrace")
        .env("BASHOPTS", "expand_aliases:extdebug:lastpipe")
        .env("BASH_COMPAT", "42")
        .env("BASH_FUNC_cd%%", FUNCTION_SOURCE)
        .env("BASH_FUNC_command_not_found_handle%%", FUNCTION_SOURCE)
        .env("CDPATH", &home)
        .env("PWD", &home)
        .env("OLDPWD", &home)
        .env("PROMPT_COMMAND", "cd \"$HOME\"")
        .env("PS4", "$(printf inherited-trace)")
        .kill_on_drop(true);
    let output = timeout(CHILD_DEADLINE, child.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "mode: {mode}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(CHILD_PASSED));
    assert!(!home.join(STARTUP_MARKER).exists());
    assert!(!home.join(FAKE_BASH_MARKER).exists());
}

#[tokio::test]
async fn isolated_startup_environment_child() {
    let Some(root) = env::var_os(FIXTURE_ROOT).map(PathBuf::from) else {
        return;
    };
    let group = ShellToolGroup::with_policy(&root, ShellPermissionPolicy::yolo())
        .await
        .unwrap();
    let prepared = group
        .prepare(ShellInput {
            command: PROBE.into(),
            timeout: Some(SHELL_TIMEOUT_MS),
            workdir: Some("work".into()),
        })
        .await
        .unwrap();
    let mode = env::var(FIXTURE_MODE).unwrap();
    if matches!(mode.as_str(), RELATIVE_MODE | MISSING_MODE) {
        assert!(prepared.bash_executable().is_none());
        assert_eq!(
            prepared.bash_command_contexts(),
            Err(BashContextError::UnsupportedLauncher)
        );
        assert!(
            group
                .execute_prepared(prepared, CancellationToken::new(), None)
                .await
                .is_err()
        );
        println!("{CHILD_PASSED}");
        return;
    }
    let executable = prepared.bash_executable().unwrap().to_owned();
    assert!(executable.is_absolute());
    assert!(!executable.starts_with(root.join("path-canary")));
    if mode == REPLACED_MODE {
        fs::rename(root.join("path-canary/bash"), &executable).unwrap();
        assert!(
            group
                .execute_prepared(prepared, CancellationToken::new(), None)
                .await
                .is_err()
        );
        println!("{CHILD_PASSED}");
        return;
    }
    if mode == RETARGETED_MODE {
        let selected = root.join("selected-bash");
        fs::remove_file(&selected).unwrap();
        symlink(root.join("path-canary/bash"), selected).unwrap();
        assert_eq!(prepared.bash_executable(), Some(executable.as_path()));
    }
    let expected = format!(
        "{}\n{}\n{}\n{}\n{PROXY_CANARY}\nunset\nunset\nunset\nunset\nunset\nunset\n",
        executable.display(),
        prepared.workdir().display(),
        env::var("HOME").unwrap(),
        env::var("PATH").unwrap()
    );
    let execution = group
        .execute_prepared(prepared, CancellationToken::new(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.output.exit_code, Some(0));
    assert_eq!(execution.output.stdout, expected);
    assert_eq!(execution.output.stderr, "");
    assert!(!execution.output.timed_out);
    println!("{CHILD_PASSED}");
}

fn input(source: &str) -> ShellInput {
    ShellInput {
        command: source.to_owned(),
        timeout: Some(SHELL_TIMEOUT_MS),
        workdir: None,
    }
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn assert_hidden_cdpath_mutation(group: &ShellToolGroup, source: &str, target: &Path) {
    let prepared = group.prepare(input(source)).await.unwrap();
    assert!(prepared.bash_program().unwrap().is_complete());
    let contexts = prepared.bash_command_contexts().unwrap();
    assert!(!contexts.complete);
    assert_eq!(
        contexts.commands.last().unwrap().incoming,
        BashCwdSet::Unknown
    );
    let execution = group
        .execute_prepared(prepared, CancellationToken::new(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.output.exit_code, Some(0));
    assert!(execution.output.stdout.contains(target.to_str().unwrap()));
    assert!(execution.output.stdout.ends_with(NOTE));
    assert!(execution.output.stderr.is_empty());
}

#[tokio::test]
async fn compgen_word_expansion_really_changes_cdpath_but_never_proves_a_known_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path().join("work");
    let outside = directory.path().join("outside");
    let target = outside.join("target");
    fs::create_dir(&work).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("note"), NOTE).unwrap();
    let group = ShellToolGroup::with_policy(&work, ShellPermissionPolicy::yolo())
        .await
        .unwrap();
    let expansion = format!("${{CDPATH:={}}}", outside.display());
    let source = format!("compgen -W {}; cd target && cat note", quoted(&expansion));
    assert_hidden_cdpath_mutation(&group, &source, &target).await;
}

#[tokio::test]
async fn a_quoted_bracket_builtin_really_mutates_cdpath_through_an_indexed_variable() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("1/target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("note"), NOTE).unwrap();
    let group = ShellToolGroup::with_policy(directory.path(), ShellPermissionPolicy::yolo())
        .await
        .unwrap();
    assert_hidden_cdpath_mutation(
        &group,
        "'[' -v 'BASH_VERSINFO[CDPATH=1]' ']'; cd target && cat note",
        &target,
    )
    .await;
}

#[tokio::test]
async fn the_running_bash_builtin_surface_is_not_mistaken_for_external_commands() {
    let directory = tempfile::tempdir().unwrap();
    let group = ShellToolGroup::with_policy(directory.path(), ShellPermissionPolicy::yolo())
        .await
        .unwrap();
    let execution = group
        .execute(input("compgen -b"), CancellationToken::new(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.output.exit_code, Some(0));
    assert!(
        execution
            .output
            .stdout
            .lines()
            .any(|name| name == "compgen")
    );
    for builtin in execution.output.stdout.lines() {
        let prepared = group
            .prepare(input(&format!("{}; cat note", quoted(builtin))))
            .await
            .unwrap();
        let contexts = prepared.bash_command_contexts().unwrap();
        let supported = matches!(builtin, ":" | "echo" | "true" | "false" | "pwd");
        assert_eq!(contexts.complete, supported, "{builtin}");
        assert_eq!(
            contexts.commands.last().unwrap().complete,
            supported,
            "{builtin}"
        );
    }
}
