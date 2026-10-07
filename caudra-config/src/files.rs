//! Every file Caudra reads configuration from. `caudra config files`, the
//! references, and the docs all list them from here.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config_file::{CONFIG_FILE, INIT_LUA_FILE};
use crate::example::{self, Document, Render};
use crate::experimental::Feature;
use crate::mcp::MCP_FILE;
use crate::providers::PROVIDERS_FILE;
use crate::sandbox::SANDBOX_FILE;
use crate::workcell::WORKCELL_PROFILE_FILE;
use crate::{ENV_FILE, PERMISSIONS_FILE, PROJECT_DIR};

pub const DOCS_ORIGIN: &str = "https://caudra.ai";
const COMMANDS_DIR: &str = "commands/";
const TOML_SUFFIX: &str = ".toml";
const EXAMPLE_COMMAND: &str = "caudra config example";
const GLOBAL: &[Scope] = &[Scope::Global];
const GLOBAL_AND_PROJECT: &[Scope] = &[Scope::Global, Scope::Project];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The config directory, for every project.
    Global,
    /// `.caudra/` in the working directory.
    Project,
}

pub struct ConfigFile {
    /// The file name, or a directory name that ends in `/`.
    pub name: &'static str,
    pub scopes: &'static [Scope],
    pub holds: &'static str,
    /// Why it is not part of caudra.toml.
    pub separate_because: &'static str,
    /// The experimental switch Caudra needs before it reads the file.
    pub feature: Option<Feature>,
    /// The docs page, as a site path.
    pub docs: &'static str,
    pub example: Option<fn() -> Document>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FileState {
    Missing,
    Present,
    /// A symlink, and the path it points at.
    Symlink(PathBuf),
    Unreadable(ErrorKind),
}

pub struct Located {
    pub file: &'static ConfigFile,
    pub scope: Scope,
    pub path: PathBuf,
    pub state: FileState,
}

pub const CAUDRA: ConfigFile = ConfigFile {
    name: CONFIG_FILE,
    scopes: GLOBAL_AND_PROJECT,
    holds: "settings, and in the global file the [experimental] switches",
    separate_because: "It is the main file, and only you write it",
    feature: None,
    docs: "/docs/configuration/",
    example: Some(example::caudra::document),
};

pub const PERMISSIONS: ConfigFile = ConfigFile {
    name: PERMISSIONS_FILE,
    scopes: GLOBAL_AND_PROJECT,
    holds: "permission rules for tools and MCP servers",
    separate_because: "It has its own error rule: a file that fails to load denies every tool call",
    feature: None,
    docs: "/docs/permissions/#toml-policy",
    example: Some(example::permissions::document),
};

pub const MCP: ConfigFile = ConfigFile {
    name: MCP_FILE,
    scopes: GLOBAL_AND_PROJECT,
    holds: "MCP servers",
    separate_because: "Caudra writes it when `/mcp` turns a server on or off, and it starts processes",
    feature: None,
    docs: "/docs/mcp/",
    example: Some(example::mcp::document),
};

pub const PROVIDERS: ConfigFile = ConfigFile {
    name: PROVIDERS_FILE,
    scopes: GLOBAL,
    holds: "model providers and their models",
    separate_because: "`caudra auth login` and `caudra auth logout` write it, and it can hold API keys",
    feature: None,
    docs: "/docs/providers/#providers-toml",
    example: Some(example::providers::document),
};

pub const WORKCELL: ConfigFile = ConfigFile {
    name: WORKCELL_PROFILE_FILE,
    scopes: GLOBAL,
    holds: "profiles for direct remote Workcell connections",
    separate_because: "It decides where credentials go, so it has to be a private file",
    feature: Some(Feature::RemoteWorkcell),
    docs: "/docs/remote-workspaces/#configure-a-profile",
    example: Some(example::workcell::document),
};

pub const SANDBOXES: ConfigFile = ConfigFile {
    name: SANDBOX_FILE,
    scopes: GLOBAL,
    holds: "managed sandbox providers, networks, transfers, and profiles",
    separate_because: "The `/sandbox` manager writes it, and it has to be a private file",
    feature: Some(Feature::Sandboxes),
    docs: "/docs/sandboxes/#configuration-schema",
    example: Some(example::sandboxes::document),
};

pub const INIT_LUA: ConfigFile = ConfigFile {
    name: INIT_LUA_FILE,
    scopes: GLOBAL_AND_PROJECT,
    holds: "Lua code that sets up plugins",
    separate_because: "It is a program, not settings",
    feature: Some(Feature::LuaPlugins),
    docs: "/docs/plugins/",
    example: None,
};

pub const ENV: ConfigFile = ConfigFile {
    name: ENV_FILE,
    scopes: GLOBAL_AND_PROJECT,
    holds: "environment variables, such as API keys, for any the environment does not set",
    separate_because: "It holds secrets",
    feature: None,
    docs: "/docs/configuration/#config-files",
    example: None,
};

pub const COMMANDS: ConfigFile = ConfigFile {
    name: COMMANDS_DIR,
    scopes: GLOBAL_AND_PROJECT,
    holds: "custom slash commands, one Markdown file each",
    separate_because: "Each command is a file of its own",
    feature: None,
    docs: "/docs/commands/#custom-commands",
    example: None,
};

pub const CONFIG_FILES: &[ConfigFile] = &[
    CAUDRA,
    PERMISSIONS,
    MCP,
    PROVIDERS,
    WORKCELL,
    SANDBOXES,
    INIT_LUA,
    ENV,
    COMMANDS,
];

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

impl ConfigFile {
    /// The name `caudra config example` takes, such as `mcp` for mcp.toml.
    pub fn stem(&self) -> &'static str {
        self.name.strip_suffix(TOML_SUFFIX).unwrap_or(self.name)
    }

    pub fn example_command(&self) -> String {
        format!("{EXAMPLE_COMMAND} {}", self.stem())
    }

    pub fn docs_url(&self) -> String {
        format!("{DOCS_ORIGIN}{}", self.docs)
    }

    /// The commented reference `caudra config example` prints.
    pub fn reference(&self) -> Option<String> {
        self.example
            .map(|document| document().render(Render::Reference))
    }

    pub fn path(&self, scope: Scope, config_dir: &Path, cwd: &Path) -> PathBuf {
        match scope {
            Scope::Global => config_dir.join(self.name),
            Scope::Project => cwd.join(PROJECT_DIR).join(self.name),
        }
    }
}

/// A file that has a reference, by stem or by name: `mcp` or `mcp.toml`.
pub fn find_example(name: &str) -> Option<&'static ConfigFile> {
    examples().find(|file| file.stem() == name || file.name == name)
}

pub fn examples() -> impl Iterator<Item = &'static ConfigFile> {
    CONFIG_FILES.iter().filter(|file| file.example.is_some())
}

/// Every place Caudra looks for configuration, global ones first, and what
/// is there. Reads metadata, never contents.
pub fn locate(config_dir: &Path, cwd: &Path) -> Vec<Located> {
    [Scope::Global, Scope::Project]
        .into_iter()
        .flat_map(|scope| {
            CONFIG_FILES
                .iter()
                .filter(move |file| file.scopes.contains(&scope))
                .map(move |file| {
                    let path = file.path(scope, config_dir, cwd);
                    let state = state(&path);
                    Located {
                        file,
                        scope,
                        path,
                        state,
                    }
                })
        })
        .collect()
}

/// Components drop a directory's trailing `/`, which would otherwise make
/// the lookup follow a symlink.
fn state(path: &Path) -> FileState {
    let path: PathBuf = path.components().collect();
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_symlink() => fs::read_link(&path).map_or_else(
            |error| FileState::Unreadable(error.kind()),
            FileState::Symlink,
        ),
        Ok(_) => FileState::Present,
        Err(error) if error.kind() == ErrorKind::NotFound => FileState::Missing,
        Err(error) => FileState::Unreadable(error.kind()),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::{CONFIG_FILES, FileState, MCP, PROVIDERS, Scope, find_example, locate};
    use crate::PROJECT_DIR;
    #[cfg(unix)]
    use crate::config_file::CONFIG_FILE;

    #[cfg(unix)]
    const SYMLINK_TARGET: &str = "shared.toml";

    #[test_case("mcp", MCP.name ; "stem")]
    #[test_case("mcp.toml", MCP.name ; "file_name")]
    #[test_case("providers", PROVIDERS.name ; "global_only_file")]
    fn an_example_is_found_by_stem_or_name(name: &str, expected: &str) {
        assert_eq!(find_example(name).map(|file| file.name), Some(expected));
    }

    #[test_case("init.lua" ; "file_without_a_reference")]
    #[test_case("commands" ; "directory")]
    fn only_toml_files_have_examples(name: &str) {
        assert!(find_example(name).is_none());
    }

    #[test]
    fn every_file_is_listed_once_per_scope() {
        let config_dir = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let located = locate(config_dir.path(), cwd.path());
        let expected: usize = CONFIG_FILES.iter().map(|file| file.scopes.len()).sum();
        assert_eq!(located.len(), expected);
        assert!(
            located
                .iter()
                .all(|entry| entry.state == FileState::Missing)
        );
        let project = cwd.path().join(PROJECT_DIR);
        assert!(
            located
                .iter()
                .filter(|entry| entry.scope == Scope::Project)
                .all(|entry| entry.path.starts_with(&project))
        );
    }

    #[cfg(unix)]
    #[test]
    fn locate_reports_present_and_symlinked_files() {
        let config_dir = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let project = cwd.path().join(PROJECT_DIR);
        fs::create_dir(&project).unwrap();
        fs::write(config_dir.path().join(CONFIG_FILE), "").unwrap();
        let target = cwd.path().join(SYMLINK_TARGET);
        symlink(&target, project.join(CONFIG_FILE)).unwrap();
        let state = |scope: Scope| {
            locate(config_dir.path(), cwd.path())
                .into_iter()
                .find(|entry| entry.scope == scope && entry.file.name == CONFIG_FILE)
                .unwrap()
                .state
        };
        assert_eq!(state(Scope::Global), FileState::Present);
        assert_eq!(state(Scope::Project), FileState::Symlink(target));
    }
}
