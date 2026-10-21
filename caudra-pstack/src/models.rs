//! pstack's per-role model choices: the file `setup-pstack` writes, Caudra's home for Cursor's
//! always-applied `pstack-models.mdc` rule.

use std::fs;
use std::path::Path;

/// Where the file lives, relative to Caudra's user config directory.
pub const ROLES_FILE: &str = "pstack/models.md";
const COMMENT: char = '#';
const BUDGET_PREFIX: &str = "# budget";
const ROLE_SEPARATOR: char = ':';

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Roles {
    Missing { path: String },
    Found { path: String, text: String },
}

impl Roles {
    /// Read fresh on every index load, so a `setup-pstack` run takes effect without a restart.
    pub fn read(config_dir: &Path) -> Self {
        let path = config_dir.join(ROLES_FILE);
        let shown = path.display().to_string();
        match fs::read_to_string(&path) {
            Ok(text) => Self::Found { path: shown, text },
            Err(_) => Self::Missing { path: shown },
        }
    }

    /// The index's section on models: the role lines in force, or how to write them.
    pub fn describe(&self) -> String {
        match self {
            Self::Missing { path } => format!(
                "No role file yet, so every role uses the model its skill names by default. \
                 `pstack/setup-pstack` writes `{path}`."
            ),
            Self::Found { path, text } => {
                let lines = role_lines(text);
                match lines.is_empty() {
                    true => format!(
                        "`{path}` sets no roles, so every role uses its skill's default."
                    ),
                    false => format!(
                        "From `{path}`. Each line overrides the skill default for its role: \
                         `<model> <thinking>` are the `task` call's `model` and `thinking`, and \
                         `inherit` omits both.\n\n```\n{}\n```",
                        lines.join("\n")
                    ),
                }
            }
        }
    }
}

/// The `# budget` line and every `role: value` line, without the other comments.
pub fn role_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with(BUDGET_PREFIX)
                || (!line.starts_with(COMMENT) && line.contains(ROLE_SEPARATOR))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/config/pstack/models.md";
    const WRITTEN: &str = "# pstack model configuration. One line per role.\n# budget: large (xhigh)\nbug-fix: grok-4.7 xhigh\njudgment and prose: claude-opus-5-5 xhigh\n";

    #[test]
    fn only_the_budget_and_role_lines_are_kept() {
        assert_eq!(
            role_lines(WRITTEN),
            [
                "# budget: large (xhigh)",
                "bug-fix: grok-4.7 xhigh",
                "judgment and prose: claude-opus-5-5 xhigh"
            ]
        );
    }

    #[test]
    fn a_missing_file_points_at_setup() {
        let roles = Roles::Missing {
            path: PATH.to_owned(),
        };
        assert!(roles.describe().contains("pstack/setup-pstack"));
    }

    #[test]
    fn a_found_file_shows_its_lines() {
        let roles = Roles::Found {
            path: PATH.to_owned(),
            text: WRITTEN.to_owned(),
        };
        assert!(roles.describe().contains("bug-fix: grok-4.7 xhigh"));
    }

    #[test]
    fn reading_finds_the_file_under_the_config_dir() {
        let config = tempfile::tempdir().unwrap();
        let missing = Roles::read(config.path());
        assert!(matches!(missing, Roles::Missing { .. }));
        let file = config.path().join(ROLES_FILE);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, WRITTEN).unwrap();
        assert!(matches!(Roles::read(config.path()), Roles::Found { text, .. } if text == WRITTEN));
    }
}
