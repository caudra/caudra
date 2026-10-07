//! `caudra config`: where Caudra looks for settings, and every setting a file
//! takes. Neither action reads a setting, so both work while one is broken.

use std::env;
use std::fmt::Write as _;
use std::io::{self, Write};

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use caudra_config::files::{self, CONFIG_FILES, ConfigFile, FileState, Located};
use caudra_storage::paths;

use crate::cli::ConfigAction;

const PRESENT: &str = "present";
const MISSING: &str = "missing";
const SYMLINK: &str = "symlink";
const UNREADABLE: &str = "unreadable";
const STATE_WIDTH: usize = UNREADABLE.len();
const NEEDS: &str = "needs";
const DOCS: &str = "docs";
const EXAMPLE: &str = "example";
const LABEL_WIDTH: usize = 9;

pub fn run(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Files => {
            let config_dir = paths::config_dir_path().context("resolve the config directory")?;
            let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
            print!("{}", listing(&files::locate(&config_dir, &cwd)));
        }
        ConfigAction::Example { file } => example(file)?,
    }
    Ok(())
}

fn example(file: &ConfigFile) -> Result<()> {
    let reference = file
        .reference()
        .ok_or_else(|| eyre!("{} has no reference", file.name))?;
    io::stdout().write_all(reference.as_bytes())?;
    Ok(())
}

/// One block per file: what it holds, the switch it needs, each place Caudra
/// looks for it, and where to read more.
fn listing(located: &[Located]) -> String {
    let mut out = String::new();
    for (index, file) in CONFIG_FILES.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "{}: {}", file.name, file.holds);
        if let Some(feature) = file.feature {
            let _ = writeln!(
                out,
                "  {NEEDS:<LABEL_WIDTH$}experimental.{} = true in the global caudra.toml",
                feature.key()
            );
        }
        for entry in located.iter().filter(|entry| entry.file.name == file.name) {
            let _ = writeln!(
                out,
                "  {:<LABEL_WIDTH$}{:<STATE_WIDTH$}  {}",
                entry.scope.label(),
                state_label(&entry.state),
                location(entry)
            );
        }
        let _ = writeln!(out, "  {DOCS:<LABEL_WIDTH$}{}", file.docs_url());
        if file.example.is_some() {
            let _ = writeln!(out, "  {EXAMPLE:<LABEL_WIDTH$}{}", file.example_command());
        }
    }
    out
}

fn state_label(state: &FileState) -> &'static str {
    match state {
        FileState::Missing => MISSING,
        FileState::Present => PRESENT,
        FileState::Symlink(_) => SYMLINK,
        FileState::Unreadable(_) => UNREADABLE,
    }
}

fn location(entry: &Located) -> String {
    let path = entry.path.display();
    match &entry.state {
        FileState::Symlink(target) => format!("{path} -> {}", target.display()),
        FileState::Unreadable(kind) => format!("{path} ({kind})"),
        FileState::Missing | FileState::Present => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::Path;

    #[cfg(unix)]
    use caudra_config::files::Scope;
    use caudra_config::files::{self, CONFIG_FILES};
    use tempfile::TempDir;

    use super::{MISSING, listing};
    #[cfg(unix)]
    use super::{PRESENT, SYMLINK};

    #[cfg(unix)]
    const SHARED_FILE: &str = "shared.toml";

    fn line_for<'a>(listing: &'a str, path: &Path) -> &'a str {
        let path = path.display().to_string();
        listing
            .lines()
            .find(|line| line.contains(&path))
            .unwrap_or_else(|| panic!("{path} is not listed"))
    }

    #[test]
    fn every_file_is_listed_with_its_switch_docs_and_example() {
        let config_dir = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let located = files::locate(config_dir.path(), cwd.path());
        let listing = listing(&located);
        for entry in &located {
            let line = line_for(&listing, &entry.path);
            assert!(
                line.contains(entry.scope.label()) && line.contains(MISSING),
                "{line}"
            );
        }
        for file in CONFIG_FILES {
            assert!(listing.contains(&file.docs_url()), "{}", file.name);
            if let Some(feature) = file.feature {
                assert!(listing.contains(&format!("experimental.{}", feature.key())));
            }
            if file.example.is_some() {
                assert!(listing.contains(&file.example_command()), "{}", file.name);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn present_and_symlinked_files_say_so() {
        let config_dir = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let global = files::CAUDRA.path(Scope::Global, config_dir.path(), cwd.path());
        let project = files::MCP.path(Scope::Project, config_dir.path(), cwd.path());
        let target = cwd.path().join(SHARED_FILE);
        fs::write(&global, "").unwrap();
        fs::create_dir_all(project.parent().unwrap()).unwrap();
        symlink(&target, &project).unwrap();
        let listing = listing(&files::locate(config_dir.path(), cwd.path()));
        assert!(line_for(&listing, &global).contains(PRESENT));
        let linked = line_for(&listing, &project);
        assert!(linked.contains(SYMLINK), "{linked}");
        assert!(linked.ends_with(&target.display().to_string()), "{linked}");
    }
}
