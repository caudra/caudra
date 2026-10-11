//! A host's builtin skill, printed by its own CLI so it describes the release
//! the model is about to call rather than whichever one Caudra was built
//! against.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::bounded_process;

const SKILL_TIMEOUT: Duration = Duration::from_secs(5);

pub type CommandFactory = Arc<dyn Fn() -> Command + Send + Sync>;

#[derive(Clone)]
pub struct SkillSource {
    pub host: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// What prints the skill.
    pub command: CommandFactory,
    /// What the model is told to do instead when nothing was printed.
    pub fallback: &'static str,
}

impl SkillSource {
    /// What the command printed, or why there is nothing, ending in the
    /// fallback.
    pub fn fetch(&self) -> Result<String, String> {
        let unavailable = format!("{} did not print its skill", self.host);
        match bounded_process::run(&mut (self.command)(), SKILL_TIMEOUT) {
            Ok(finished) if finished.status.success() => {
                Ok(String::from_utf8_lossy(&finished.stdout).into_owned())
            }
            Ok(finished) => Err(format!(
                "{unavailable} ({}): {}. {}",
                finished.status,
                String::from_utf8_lossy(finished.stderr.trim_ascii()),
                self.fallback
            )),
            Err(error) => Err(format!("{unavailable}: {error}. {}", self.fallback)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const HOST: &str = "Host";
    const BODY: &str = "# Host\n\nRun host pane list.\n";
    const FALLBACK: &str = "Run `host --help`.";

    fn source(dir: &Path, script: &str) -> SkillSource {
        let binary = dir.join("host");
        fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        SkillSource {
            host: HOST,
            name: "host",
            description: "Control the host.",
            command: Arc::new(move || Command::new(&binary)),
            fallback: FALLBACK,
        }
    }

    #[test]
    fn the_skill_is_what_the_command_prints() {
        let dir = TempDir::new().unwrap();

        let fetched = source(dir.path(), &format!("printf '{BODY}'")).fetch();

        assert_eq!(fetched.unwrap(), BODY);
    }

    #[test_case("echo 'unknown option: --skill' >&2; exit 2" ; "older_host")]
    #[test_case("exit 1" ; "failing_host")]
    fn a_host_without_a_skill_says_so_and_what_to_do(script: &str) {
        let dir = TempDir::new().unwrap();

        let error = source(dir.path(), script).fetch().unwrap_err();

        assert!(
            error.starts_with(&format!("{HOST} did not print its skill")),
            "{error}"
        );
        assert!(error.ends_with(FALLBACK), "{error}");
    }
}
