//! The `herdr` builtin skill, offered inside a Herdr pane. Its body is what the
//! installed Herdr prints for `herdr --skill`, so it describes the CLI the model
//! is about to call rather than whichever release Caudra was built against.

use std::ffi::OsString;
use std::time::Duration;

use super::env::HerdrEnv;
use crate::bounded_process;
use crate::tools::native::skill::{BuiltinSkill, parse_frontmatter};

const HERDR_SKILL: &str = "herdr";
const SKILL_FLAG: &str = "--skill";
const SKILL_TIMEOUT: Duration = Duration::from_secs(5);
const DESCRIPTION: &str = "Control Herdr, the terminal multiplexer this session runs in: inspect and drive its workspaces, tabs, panes, commands, and the agents running in them. Use only when the user asks for Herdr.";
const UNAVAILABLE: &str = "Herdr did not print its skill";
const FALLBACK: &str = "Run `herdr --help` to learn the CLI instead.";

/// A disk skill of the same name still wins, like every builtin.
pub fn herdr_skill(herdr: HerdrEnv) -> BuiltinSkill {
    BuiltinSkill {
        name: HERDR_SKILL.to_owned(),
        description: DESCRIPTION.to_owned(),
        resolve: Box::new(move || (body(&herdr), None)),
    }
}

/// What `herdr --skill` prints without its header, or why there is nothing.
fn body(herdr: &HerdrEnv) -> String {
    let mut command = herdr.cli().command(&[OsString::from(SKILL_FLAG)]);
    match bounded_process::run(&mut command, SKILL_TIMEOUT) {
        Ok(finished) if finished.status.success() => {
            parse_frontmatter(&String::from_utf8_lossy(&finished.stdout)).1
        }
        Ok(finished) => format!(
            "{UNAVAILABLE} ({}): {}. {FALLBACK}",
            finished.status,
            String::from_utf8_lossy(finished.stderr.trim_ascii())
        ),
        Err(error) => format!("{UNAVAILABLE}: {error}. {FALLBACK}"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::{HERDR_SKILL, UNAVAILABLE, herdr_skill};
    use crate::herdr::HerdrEnv;

    const BODY: &str = "# Herdr\n\nRun herdr pane list.";
    const PANE: &str = "w1:p2";
    const SOCKET: &str = "/tmp/herdr.sock";

    fn fake_herdr(dir: &Path, script: &str) -> HerdrEnv {
        let binary = dir.join("herdr");
        fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        HerdrEnv {
            binary: binary.into(),
            socket_path: SOCKET.into(),
            pane_id: PANE.into(),
            workspace_id: None,
        }
    }

    #[test]
    fn the_body_is_what_the_installed_herdr_prints() {
        let dir = TempDir::new().unwrap();
        let herdr = fake_herdr(
            dir.path(),
            &format!(
                "[ \"$1\" = --skill ] && printf -- '---\\nname: herdr\\ndescription: x\\n---\\n\\n%s\\n' '{BODY}'"
            ),
        );

        let skill = herdr_skill(herdr);
        let (body, reference) = (skill.resolve)();

        assert_eq!(skill.name, HERDR_SKILL);
        assert_eq!(body, BODY);
        assert!(reference.is_none());
    }

    #[test_case("echo 'unknown option: --skill' >&2; exit 2" ; "older_herdr")]
    #[test_case("exit 1" ; "failing_herdr")]
    fn a_herdr_without_a_skill_says_so(script: &str) {
        let dir = TempDir::new().unwrap();

        let (body, _) = (herdr_skill(fake_herdr(dir.path(), script)).resolve)();

        assert!(body.starts_with(UNAVAILABLE), "{body}");
    }
}
