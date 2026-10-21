//! Discovers the automation scripts a session may arm: `<project>/.caudra/automations`, then the
//! user's `<config>/automations`, then the [`bundled`](super::bundled) ones the binary ships. The
//! catalog keeps one entry per name: an earlier scope's script hides a later one of the same name
//! and records its scope in `shadowed`. A script that breaks a rule stays listed as invalid, with
//! the reason.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use caudra_automation::catalog::{
    AUTOMATION_ABI_VERSION, AUTOMATION_LANGUAGE_VERSION, AutomationCatalog, CatalogEntry,
    InvalidEntry, Scope, Trust, check_entry,
};
use caudra_automation::meta::{
    AutomationMeta, BROADCAST, MAX_SOURCE_BYTES, META_VARIABLE, MessagingCaps, Trigger,
    TriggerKind, parse_meta,
};
use caudra_config::{Feature, FeatureFlags};
use caudra_storage::digest_trust::TrustDomain;
use caudra_storage::paths::{config_dir, user_config_dir};
use caudra_storage::topics::{parse_pattern, parse_topic};
use caudra_storage::workflow_source::{SourceReadError, read_bounded_regular_file};
use caudra_storage::{StateDir, StorageError};
use thiserror::Error;
use tracing::{debug, warn};

use super::bundled::{BundledAutomation, bundled_automations};

const PROJECT_DIR: &str = ".caudra";
pub(crate) const AUTOMATIONS_SUBDIR: &str = "automations";
pub(crate) const SCRIPT_EXTENSION: &str = "rhai";
const TRUST_DOMAIN: TrustDomain = TrustDomain::Automation;
const NOT_UTF8: &str = "source is not valid UTF-8";
const NOT_A_DIRECTORY: &str = "is not a real directory";
const NEEDS_FEATURE: &str = "needs";
pub const UNAVAILABLE_IN_SDK: &str = "is unavailable in SDK sessions";
const MESSAGING_FIELD: &str = "messaging";
/// The triggers an SDK session cannot serve: it shows no prompt for `needs_input` to wait on,
/// and opens no peer session for messages or group work.
const SDK_REFUSED_TRIGGERS: [(TriggerKind, &str); 3] = [
    (TriggerKind::NeedsInput, "needs_input"),
    (TriggerKind::MessageReceived, "message_received"),
    (TriggerKind::WorkFinished, "work_finished"),
];

/// A loadable script. `source` is the exact text hashed into `entry.digest`, so what the user
/// trusted is what arms.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAutomation {
    pub entry: CatalogEntry,
    pub source: String,
}

#[derive(Debug, Default)]
pub struct Catalog {
    /// One per name, the project scope's first.
    entries: Vec<ResolvedAutomation>,
    invalid: Vec<InvalidEntry>,
    /// The invalid names that only this frontend refuses: another one serves them.
    unavailable: Vec<String>,
    project_root: Option<PathBuf>,
}

/// Where the session runs, which decides the triggers and capabilities its scripts may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Tui,
    /// Shows no question form or permission prompt to wait on, and opens no peer session.
    Sdk,
}

/// The directories a scan reads, as resolved for this machine and build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutomationDirs {
    /// `None` for a remote session, whose project lives on another machine, or when the working
    /// directory does not resolve.
    pub project: Option<PathBuf>,
    /// `None` without a user config directory.
    pub user: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("unknown automation {name:?}")]
    Unknown { name: String },
    #[error("automation {name:?} at {} is invalid: {reason}", path.display())]
    Invalid {
        name: String,
        path: PathBuf,
        reason: String,
    },
    #[error(
        "automation {name:?} at {} changed after it was reviewed (digest {actual}, not {expected}); review it again before trusting it",
        path.display()
    )]
    Changed {
        name: String,
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("cannot read automation {name:?} again to trust it: {error}")]
    Reread {
        name: String,
        error: SourceReadError,
    },
    #[error("automation {name:?} at {} is outside the project root", path.display())]
    OutsideProject { name: String, path: PathBuf },
    #[error("cannot record trust in automation {name:?}: {error}")]
    Storage { name: String, error: StorageError },
}

impl Catalog {
    pub fn scan(
        state_dir: &StateDir,
        cwd: &Path,
        features: FeatureFlags,
        frontend: Frontend,
    ) -> Self {
        Self::scan_with(
            state_dir,
            cwd,
            config_dir().ok().as_deref(),
            &bundled_automations(),
            features,
            frontend,
        )
    }

    /// `user_config` is the user's config directory; production passes [`config_dir`], tests
    /// pass a tempdir. `bundled` is what production has installed, and what a test lists.
    /// `features` gates the scripts that use messaging or workflows, and `frontend` the ones
    /// that need what only the TUI serves.
    pub fn scan_with(
        state_dir: &StateDir,
        cwd: &Path,
        user_config: Option<&Path>,
        bundled: &[BundledAutomation],
        features: FeatureFlags,
        frontend: Frontend,
    ) -> Self {
        let mut catalog = Self {
            project_root: cwd
                .canonicalize()
                .map_err(
                    |error| debug!(cwd = %cwd.display(), %error, "no project automation scope"),
                )
                .ok(),
            ..Self::default()
        };
        if let Some(root) = catalog.project_root.clone() {
            catalog.scan_directory(
                &project_dir(&root),
                Scope::Project,
                features,
                frontend,
                |path, digest| project_trust(state_dir, &root, path, digest),
            );
        }
        catalog.scan_user_scope(user_config, features, frontend);
        catalog.scan_bundled(bundled, features, frontend);
        catalog
    }

    /// For remote sessions, whose project lives on another machine: only the user's own
    /// scripts and the bundled ones apply.
    pub fn scan_user_only(
        user_config: Option<&Path>,
        bundled: &[BundledAutomation],
        features: FeatureFlags,
        frontend: Frontend,
    ) -> Self {
        let mut catalog = Self::default();
        catalog.scan_user_scope(user_config, features, frontend);
        catalog.scan_bundled(bundled, features, frontend);
        catalog
    }

    /// Whether `name` is invalid only because this frontend cannot serve it.
    pub fn unavailable_here(&self, name: &str) -> bool {
        self.find(name).is_none()
            && self
                .unavailable
                .iter()
                .any(|unavailable| unavailable == name)
    }

    pub fn to_catalog(&self) -> AutomationCatalog {
        AutomationCatalog {
            entries: self
                .entries
                .iter()
                .map(|resolved| resolved.entry.clone())
                .collect(),
            invalid: self.invalid.clone(),
        }
    }

    pub fn resolve(&self, name: &str) -> Result<ResolvedAutomation, CatalogError> {
        self.lookup(name).cloned()
    }

    /// One loadable script per name, the project scope's first.
    pub fn entries(&self) -> &[ResolvedAutomation] {
        &self.entries
    }

    pub fn invalid(&self) -> &[InvalidEntry] {
        &self.invalid
    }

    pub fn find(&self, name: &str) -> Option<&ResolvedAutomation> {
        self.entries
            .iter()
            .find(|resolved| resolved.entry.meta.name == name)
    }

    /// Records trust for a project script, provided the file still holds the bytes the caller
    /// approved. User scripts are trusted where they live, so this is a no-op for them.
    pub fn trust(
        &self,
        state_dir: &StateDir,
        name: &str,
        expected_digest: &str,
    ) -> Result<(), CatalogError> {
        let entry = &self.lookup(name)?.entry;
        if entry.scope != Scope::Project {
            return Ok(());
        }
        let bytes = read_bounded_regular_file(&entry.path, MAX_SOURCE_BYTES).map_err(|error| {
            CatalogError::Reread {
                name: name.to_owned(),
                error,
            }
        })?;
        let actual = digest_of(&bytes);
        if actual != expected_digest {
            return Err(CatalogError::Changed {
                name: name.to_owned(),
                path: entry.path.clone(),
                expected: expected_digest.to_owned(),
                actual,
            });
        }
        let outside = || CatalogError::OutsideProject {
            name: name.to_owned(),
            path: entry.path.clone(),
        };
        let root = self.project_root.as_deref().ok_or_else(outside)?;
        let relative = project_relative(root, &entry.path).ok_or_else(outside)?;
        TRUST_DOMAIN
            .trust(state_dir, root, relative, &actual)
            .map_err(|error| CatalogError::Storage {
                name: name.to_owned(),
                error,
            })
    }

    fn lookup(&self, name: &str) -> Result<&ResolvedAutomation, CatalogError> {
        self.find(name).ok_or_else(|| {
            match self.invalid.iter().find(|invalid| invalid.name == name) {
                Some(invalid) => CatalogError::Invalid {
                    name: name.to_owned(),
                    path: invalid.path.clone(),
                    reason: invalid.reason.clone(),
                },
                None => CatalogError::Unknown {
                    name: name.to_owned(),
                },
            }
        })
    }

    fn scan_user_scope(
        &mut self,
        user_config: Option<&Path>,
        features: FeatureFlags,
        frontend: Frontend,
    ) {
        if let Some(dir) = user_config_dir(user_config, AUTOMATIONS_SUBDIR) {
            self.scan_directory(&dir, Scope::User, features, frontend, |_, _| false);
        }
    }

    /// `approved` reports whether trust in a digest was recorded for the file.
    fn scan_directory(
        &mut self,
        dir: &Path,
        scope: Scope,
        features: FeatureFlags,
        frontend: Frontend,
        approved: impl Fn(&Path, &str) -> bool,
    ) {
        let paths = match script_paths(dir) {
            Ok(paths) => paths,
            Err(reason) => {
                self.invalid.push(invalid_entry(dir, scope, reason));
                return;
            }
        };
        for path in paths {
            let loaded = load_script(&path, scope, features, &approved);
            self.place(loaded, &path, scope, frontend);
        }
    }

    /// Trusted as shipped, so no approval is looked up.
    fn scan_bundled(
        &mut self,
        bundled: &[BundledAutomation],
        features: FeatureFlags,
        frontend: Frontend,
    ) {
        for script in bundled {
            let path = script.path();
            let resolved = resolve_source(
                &script.name,
                &path,
                script.source.to_string(),
                Scope::Bundled,
                features,
                |_, _| false,
            );
            self.place(resolved, &path, Scope::Bundled, frontend);
        }
    }

    /// Lists a loaded script, or the reason it cannot run here.
    fn place(
        &mut self,
        loaded: Result<ResolvedAutomation, String>,
        path: &Path,
        scope: Scope,
        frontend: Frontend,
    ) {
        match loaded {
            Ok(resolved) => match check_frontend(&resolved.entry.meta, frontend) {
                Ok(()) => self.admit(resolved),
                Err(reason) => {
                    self.unavailable.push(resolved.entry.meta.name);
                    self.invalid.push(invalid_entry(path, scope, reason));
                }
            },
            Err(reason) => self.invalid.push(invalid_entry(path, scope, reason)),
        }
    }

    /// Scopes are scanned in precedence order, so an earlier entry of the same name wins.
    fn admit(&mut self, resolved: ResolvedAutomation) {
        let name = &resolved.entry.meta.name;
        match self
            .entries
            .iter_mut()
            .find(|winner| winner.entry.meta.name == *name)
        {
            Some(winner) => winner.entry.shadowed.push(resolved.entry.scope),
            None => self.entries.push(resolved),
        }
    }
}

impl AutomationDirs {
    /// `cwd` is the session's working directory, `None` for a remote session; `user_config` is
    /// the user's config directory, as [`Catalog::scan_with`] takes them.
    pub fn resolve(cwd: Option<&Path>, user_config: Option<&Path>) -> Self {
        Self {
            project: cwd
                .and_then(|cwd| cwd.canonicalize().ok())
                .map(|root| project_dir(&root)),
            user: user_config_dir(user_config, AUTOMATIONS_SUBDIR),
        }
    }
}

fn project_dir(root: &Path) -> PathBuf {
    root.join(PROJECT_DIR).join(AUTOMATIONS_SUBDIR)
}

/// The `.rhai` files directly inside `dir`, sorted by name. A missing directory is an empty
/// scope; anything else that is not a real directory is an error.
fn script_paths(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NOT_A_DIRECTORY.to_owned());
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()
        })
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == SCRIPT_EXTENSION)
        })
        .collect();
    paths.sort();
    Ok(paths)
}

/// Reads the file once; the same buffer is hashed, parsed, and kept as the source that arms.
fn load_script(
    path: &Path,
    scope: Scope,
    features: FeatureFlags,
    approved: impl Fn(&Path, &str) -> bool,
) -> Result<ResolvedAutomation, String> {
    let bytes =
        read_bounded_regular_file(path, MAX_SOURCE_BYTES).map_err(|error| error.to_string())?;
    let source = String::from_utf8(bytes).map_err(|_| NOT_UTF8.to_owned())?;
    resolve_source(&stem(path), path, source, scope, features, approved)
}

/// Checks a script's header against the name it is listed under, and keeps `source` as the text
/// its digest covers.
fn resolve_source(
    name: &str,
    path: &Path,
    source: String,
    scope: Scope,
    features: FeatureFlags,
    approved: impl Fn(&Path, &str) -> bool,
) -> Result<ResolvedAutomation, String> {
    let meta = parse_meta(&source).map_err(|error| error.to_string())?;
    let warnings = check_entry(name, scope, &meta)?;
    check_topics(&meta)?;
    check_features(&meta, features)?;
    let digest = digest_of(source.as_bytes());
    Ok(ResolvedAutomation {
        entry: CatalogEntry {
            trust: Trust::assess(scope, approved(path, &digest)),
            meta,
            scope,
            path: path.to_path_buf(),
            digest,
            warnings,
            shadowed: Vec::new(),
        },
        source,
    })
}

/// The header keeps topics opaque, so the messaging grammar is applied here.
fn check_topics(meta: &AutomationMeta) -> Result<(), String> {
    for (index, trigger) in meta.triggers.iter().enumerate() {
        let Trigger::MessageReceived(filter) = trigger else {
            continue;
        };
        for (position, pattern) in filter.topics.iter().enumerate() {
            parse_pattern(pattern).map_err(|reason| {
                format!("{META_VARIABLE}.triggers[{index}].topics[{position}]: {reason}")
            })?;
        }
    }
    for (position, topic) in meta.messaging.publish.iter().enumerate() {
        if topic != BROADCAST {
            parse_topic(topic).map_err(|reason| {
                format!("{META_VARIABLE}.messaging.publish[{position}]: {reason}")
            })?;
        }
    }
    Ok(())
}

/// The switches besides `automations` that a script needs: cross-session messaging for its
/// messaging triggers and capabilities, and workflows for `workflow_finished` and the workflows
/// it starts.
pub fn needed_features(meta: &AutomationMeta) -> Vec<Feature> {
    let messaging = meta.messaging != MessagingCaps::default()
        || meta.triggers.iter().any(|trigger| {
            matches!(
                trigger,
                Trigger::MessageReceived(_) | Trigger::WorkFinished { .. }
            )
        });
    let workflows = !meta.workflows.is_empty()
        || meta
            .triggers
            .iter()
            .any(|trigger| matches!(trigger, Trigger::WorkflowFinished { .. }));
    [
        (Feature::CrossSessionMessaging, messaging),
        (Feature::Workflows, workflows),
    ]
    .into_iter()
    .filter_map(|(feature, used)| used.then_some(feature))
    .collect()
}

/// Refuses a script that uses messaging or workflows while that feature is turned off.
fn check_features(meta: &AutomationMeta, features: FeatureFlags) -> Result<(), String> {
    needed_features(meta)
        .into_iter()
        .find(|&feature| !features.enabled(feature))
        .map_or(Ok(()), |feature| Err(format!("{NEEDS_FEATURE} {feature}")))
}

/// Refuses, in an SDK session, a script that needs what only the TUI serves, naming what.
fn check_frontend(meta: &AutomationMeta, frontend: Frontend) -> Result<(), String> {
    if frontend == Frontend::Tui {
        return Ok(());
    }
    for (index, trigger) in meta.triggers.iter().enumerate() {
        if let Some((_, kind)) = SDK_REFUSED_TRIGGERS
            .iter()
            .find(|(refused, _)| *refused == trigger.kind())
        {
            return Err(format!(
                "{META_VARIABLE}.triggers[{index}]: {kind} {UNAVAILABLE_IN_SDK}"
            ));
        }
    }
    if meta.messaging != MessagingCaps::default() {
        return Err(format!(
            "{META_VARIABLE}.{MESSAGING_FIELD} {UNAVAILABLE_IN_SDK}"
        ));
    }
    Ok(())
}

fn invalid_entry(path: &Path, scope: Scope, reason: String) -> InvalidEntry {
    InvalidEntry {
        name: stem(path),
        scope,
        path: path.to_path_buf(),
        reason,
    }
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn digest_of(bytes: &[u8]) -> String {
    TRUST_DOMAIN.source_digest(bytes, AUTOMATION_LANGUAGE_VERSION, AUTOMATION_ABI_VERSION)
}

fn project_relative<'a>(root: &Path, path: &'a Path) -> Option<&'a str> {
    path.strip_prefix(root).ok()?.to_str()
}

fn project_trust(state_dir: &StateDir, root: &Path, path: &Path, digest: &str) -> bool {
    let Some(relative) = project_relative(root, path) else {
        return false;
    };
    TRUST_DOMAIN
        .is_trusted(state_dir, root, relative, digest)
        .unwrap_or_else(|error| {
            warn!(path = %path.display(), %error, "automation trust lookup failed");
            false
        })
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::slice;

    use caudra_automation::catalog::{
        ALWAYS_OUTSIDE_USER_SCOPE, ALWAYS_WITH_SCHEDULE, STEM_MISMATCH,
    };
    use caudra_automation::meta::MetaError;
    use caudra_storage::topics::{INVALID_PATTERN, INVALID_TOPIC};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::automation::bundled::{install_bundled_automations, remove_bundled_automations};

    const PROVIDER: &str = "catalog-test";
    /// Installed in the process-wide registry, so no other test lists it.
    const REGISTRY_PROVIDER: &str = "catalog-registry-test";
    const BUNDLED_NAME: &str = "shipped";
    const BUNDLED_SOURCE: &str = "let meta = #{ name: \"shipped\", description: \"Poll CI\", triggers: [#{ kind: \"armed\" }] };\n";
    const BUNDLED_HAS_NO_FILE: &str = "a bundled script's path names its provider, not a file";
    const BUNDLED_IS_TRUSTED: &str = "a bundled script is trusted as shipped";
    const NAME: &str = "ci-watch";
    const OTHER_NAME: &str = "standup";
    const MISMATCHED_STEM: &str = "other";
    const DESCRIPTION: &str = "Poll CI";
    const ARMED: &str = r#"triggers: [#{ kind: "armed" }]"#;
    const ALWAYS_ON_SCHEDULE: &str =
        r#"triggers: [#{ kind: "schedule", every: "10m" }], arm: "always""#;
    const BAD_TOPIC_PATTERN: &str = r#"triggers: [#{ kind: "armed" }, #{ kind: "message_received", topics: ["ci.*", "CI.*"] }]"#;
    const BAD_PUBLISH_TOPIC: &str =
        r#"triggers: [#{ kind: "armed" }], messaging: #{ publish: ["broadcast", "CI"] }"#;
    const NOT_FIRST: &[u8] = b"let x = 1;";
    const SPACE: u8 = b' ';
    const USER_IS_TRUSTED: &str = "a user script needs no approval";
    const TRUST_IS_EXACT: &str = "trust must follow the approved bytes exactly";
    const STALE_IS_REFUSED: &str = "approving bytes that are no longer on disk must record nothing";
    const PROJECT_WINS: &str = "the project scope must take precedence over the user scope";
    const ONE_ENTRY_PER_NAME: &str = "a hidden script must be recorded on its winner, not listed";
    const PROJECT_IS_IGNORED: &str = "a user-only scan must not read the project scope";
    const NOTHING_INVALID: &str = "missing directories must not be reported as errors";
    const PROJECT_DIR_RESOLVES: &str = "a local session must resolve its project directory";
    const USER_DIR_RESOLVES: &str = "a config directory must resolve the user directory";
    const REMOTE_HAS_NO_PROJECT: &str = "a remote session must name no project directory";

    struct Fixture {
        _temp: TempDir,
        state_dir: StateDir,
        project: PathBuf,
        config: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let project = temp.path().join("project");
            let config = temp.path().join("config");
            fs::create_dir(&project).unwrap();
            fs::create_dir(&config).unwrap();
            Self {
                state_dir: StateDir::from_path(temp.path().join("state")),
                project: project.canonicalize().unwrap(),
                config,
                _temp: temp,
            }
        }

        fn project_dir(&self) -> PathBuf {
            self.project.join(PROJECT_DIR).join(AUTOMATIONS_SUBDIR)
        }

        fn user_dir(&self) -> PathBuf {
            self.config.join(AUTOMATIONS_SUBDIR)
        }

        /// Writes `source` as `name`'s script in `scope`, or returns it bundled.
        fn place(&self, scope: Scope, name: &str, source: &str) -> Option<BundledAutomation> {
            let dir = match scope {
                Scope::Project => self.project_dir(),
                Scope::User => self.user_dir(),
                Scope::Bundled => return Some(bundled(name, source)),
            };
            self.write(&dir, &file_name(name), source.as_bytes());
            None
        }

        fn write(&self, dir: &Path, file: &str, content: &[u8]) -> PathBuf {
            fs::create_dir_all(dir).unwrap();
            let path = dir.join(file);
            fs::write(&path, content).unwrap();
            path
        }

        fn scan(&self, features: FeatureFlags) -> Catalog {
            self.scan_in(features, Frontend::Tui)
        }

        fn scan_in(&self, features: FeatureFlags, frontend: Frontend) -> Catalog {
            Catalog::scan_with(
                &self.state_dir,
                &self.project,
                Some(&self.config),
                &[],
                features,
                frontend,
            )
        }

        fn scan_bundled(&self, bundled: &[BundledAutomation]) -> Catalog {
            Catalog::scan_with(
                &self.state_dir,
                &self.project,
                Some(&self.config),
                bundled,
                FeatureFlags::NONE,
                Frontend::Tui,
            )
        }

        fn trust_of(&self, name: &str) -> Trust {
            self.scan(FeatureFlags::NONE)
                .resolve(name)
                .unwrap()
                .entry
                .trust
        }
    }

    fn script(name: &str, fields: &str) -> String {
        format!("let meta = #{{ name: \"{name}\", description: \"{DESCRIPTION}\", {fields} }};\n")
    }

    fn file_name(name: &str) -> String {
        format!("{name}.{SCRIPT_EXTENSION}")
    }

    fn bundled(name: &str, source: &str) -> BundledAutomation {
        BundledAutomation {
            provider: PROVIDER,
            name: name.to_owned(),
            source: Cow::Owned(source.to_owned()),
        }
    }

    fn names(catalog: &AutomationCatalog) -> Vec<&str> {
        catalog
            .entries
            .iter()
            .map(|entry| entry.meta.name.as_str())
            .collect()
    }

    #[test]
    fn a_user_script_is_listed_and_trusted_by_location() {
        let fixture = Fixture::new();
        let source = script(NAME, ARMED);
        let path = fixture.write(&fixture.user_dir(), &file_name(NAME), source.as_bytes());

        let catalog = fixture.scan(FeatureFlags::NONE);
        let resolved = catalog.resolve(NAME).unwrap();

        let listed = catalog.to_catalog();
        assert_eq!(names(&listed), [NAME]);
        assert!(listed.invalid.is_empty());
        assert_eq!(resolved.source, source);
        assert_eq!(resolved.entry.meta, parse_meta(&source).unwrap());
        assert_eq!(resolved.entry.scope, Scope::User);
        assert_eq!(resolved.entry.path, path);
        assert_eq!(resolved.entry.digest, digest_of(source.as_bytes()));
        assert_eq!(resolved.entry.trust, Trust::Location, "{USER_IS_TRUSTED}");
        assert!(resolved.entry.warnings.is_empty());
        assert!(matches!(
            catalog.trust(&fixture.state_dir, NAME, &resolved.entry.digest),
            Ok(())
        ));
    }

    #[test]
    fn the_resolved_directories_are_the_ones_the_scan_reads() {
        let fixture = Fixture::new();
        let dirs = AutomationDirs::resolve(Some(&fixture.project), Some(&fixture.config));
        let project = dirs.project.expect(PROJECT_DIR_RESOLVES);
        let user = dirs.user.expect(USER_DIR_RESOLVES);
        fixture.write(&project, &file_name(NAME), script(NAME, ARMED).as_bytes());
        fixture.write(
            &user,
            &file_name(OTHER_NAME),
            script(OTHER_NAME, ARMED).as_bytes(),
        );

        let catalog = fixture.scan(FeatureFlags::NONE);

        assert_eq!(catalog.resolve(NAME).unwrap().entry.scope, Scope::Project);
        assert_eq!(
            catalog.resolve(OTHER_NAME).unwrap().entry.scope,
            Scope::User
        );
        assert_eq!(
            AutomationDirs::resolve(None, Some(&fixture.config)),
            AutomationDirs {
                project: None,
                user: Some(user),
            },
            "{REMOTE_HAS_NO_PROJECT}"
        );
    }

    #[test]
    fn a_project_script_is_trusted_by_its_exact_digest() {
        let fixture = Fixture::new();
        let source = script(NAME, ARMED);
        let path = fixture.write(&fixture.project_dir(), &file_name(NAME), source.as_bytes());

        let catalog = fixture.scan(FeatureFlags::NONE);
        let reviewed = catalog.resolve(NAME).unwrap().entry;

        assert_eq!(reviewed.scope, Scope::Project);
        assert_eq!(reviewed.trust, Trust::Required);
        catalog
            .trust(&fixture.state_dir, NAME, &reviewed.digest)
            .unwrap();
        assert_eq!(fixture.trust_of(NAME), Trust::Approved);

        let mut edited = source.into_bytes();
        *edited.last_mut().unwrap() = SPACE;
        fs::write(&path, edited).unwrap();
        assert_eq!(fixture.trust_of(NAME), Trust::Required, "{TRUST_IS_EXACT}");
    }

    #[test]
    fn trusting_a_stale_digest_is_refused() {
        let fixture = Fixture::new();
        let source = script(NAME, ARMED);
        let path = fixture.write(&fixture.project_dir(), &file_name(NAME), source.as_bytes());
        let catalog = fixture.scan(FeatureFlags::NONE);
        let reviewed = catalog.resolve(NAME).unwrap().entry.digest;
        let edited = format!("{source}// edited after review\n");
        fs::write(&path, &edited).unwrap();

        let error = catalog
            .trust(&fixture.state_dir, NAME, &reviewed)
            .unwrap_err();

        assert!(
            matches!(
                &error,
                CatalogError::Changed { expected, actual, .. }
                    if *expected == reviewed && *actual == digest_of(edited.as_bytes())
            ),
            "{error}"
        );
        fs::write(&path, &source).unwrap();
        assert_eq!(
            fixture.trust_of(NAME),
            Trust::Required,
            "{STALE_IS_REFUSED}"
        );
    }

    #[test_case(&file_name(NAME), vec![0xff, 0xfe], NOT_UTF8; "invalid_utf8")]
    #[test_case(&file_name(MISMATCHED_STEM), script(NAME, ARMED).into_bytes(), STEM_MISMATCH; "stem_mismatch")]
    #[test_case(&file_name(NAME), NOT_FIRST.to_vec(), &MetaError::NotFirst.to_string(); "invalid_header")]
    #[test_case(&file_name(NAME), script(NAME, BAD_TOPIC_PATTERN).into_bytes(), &format!("{META_VARIABLE}.triggers[1].topics[1]: {INVALID_PATTERN}"); "invalid_topic_pattern")]
    #[test_case(&file_name(NAME), script(NAME, BAD_PUBLISH_TOPIC).into_bytes(), &format!("{META_VARIABLE}.messaging.publish[1]: {INVALID_TOPIC}"); "invalid_publish_topic")]
    fn a_broken_script_is_listed_invalid(file: &str, content: Vec<u8>, expected: &str) {
        let fixture = Fixture::new();
        let path = fixture.write(&fixture.user_dir(), file, &content);

        let catalog = fixture.scan(FeatureFlags::all());

        let listed = catalog.to_catalog();
        assert!(listed.entries.is_empty());
        let [invalid] = listed.invalid.as_slice() else {
            panic!("{:?}", listed.invalid);
        };
        assert_eq!(invalid.name, stem(&path));
        assert_eq!(invalid.scope, Scope::User);
        assert_eq!(invalid.path, path);
        assert!(invalid.reason.starts_with(expected), "{}", invalid.reason);
        assert!(matches!(
            catalog.resolve(&invalid.name),
            Err(CatalogError::Invalid { reason, .. }) if reason == invalid.reason
        ));
    }

    #[test]
    fn an_oversized_script_is_listed_invalid() {
        let fixture = Fixture::new();
        let size = MAX_SOURCE_BYTES + 1;
        let path = fixture.write(&fixture.user_dir(), &file_name(NAME), &vec![SPACE; size]);

        let listed = fixture.scan(FeatureFlags::all()).to_catalog();

        assert!(listed.entries.is_empty());
        assert_eq!(
            listed.invalid,
            [InvalidEntry {
                name: NAME.into(),
                scope: Scope::User,
                path: path.clone(),
                reason: SourceReadError::TooLarge {
                    path,
                    actual: u64::try_from(size).unwrap(),
                    max: MAX_SOURCE_BYTES,
                }
                .to_string(),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_script_is_listed_invalid() {
        let fixture = Fixture::new();
        let target = fixture.write(
            &fixture.config,
            &file_name(NAME),
            script(NAME, ARMED).as_bytes(),
        );
        let dir = fixture.project_dir();
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join(file_name(NAME));
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let listed = fixture.scan(FeatureFlags::all()).to_catalog();

        assert!(listed.entries.is_empty());
        assert_eq!(
            listed.invalid,
            [InvalidEntry {
                name: NAME.into(),
                scope: Scope::Project,
                path: link.clone(),
                reason: SourceReadError::Symlink { path: link }.to_string(),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_automations_directory_is_listed_invalid() {
        let fixture = Fixture::new();
        fixture.write(
            &fixture.config,
            &file_name(NAME),
            script(NAME, ARMED).as_bytes(),
        );
        fs::create_dir_all(fixture.project.join(PROJECT_DIR)).unwrap();
        let dir = fixture.project_dir();
        std::os::unix::fs::symlink(&fixture.config, &dir).unwrap();

        let listed = fixture.scan(FeatureFlags::all()).to_catalog();

        assert!(listed.entries.is_empty());
        assert_eq!(
            listed.invalid,
            [InvalidEntry {
                name: AUTOMATIONS_SUBDIR.into(),
                scope: Scope::Project,
                path: dir,
                reason: NOT_A_DIRECTORY.into(),
            }]
        );
    }

    #[test_case(r#"triggers: [#{ kind: "message_received" }]"#, Feature::CrossSessionMessaging; "message_received")]
    #[test_case(r#"triggers: [#{ kind: "work_finished" }]"#, Feature::CrossSessionMessaging; "work_finished")]
    #[test_case(r#"triggers: [#{ kind: "armed" }], messaging: #{ reply: true }"#, Feature::CrossSessionMessaging; "reply")]
    #[test_case(r#"triggers: [#{ kind: "armed" }], messaging: #{ send: ["*"] }"#, Feature::CrossSessionMessaging; "send")]
    #[test_case(r#"triggers: [#{ kind: "armed" }], messaging: #{ publish: ["broadcast"] }"#, Feature::CrossSessionMessaging; "publish")]
    #[test_case(r#"triggers: [#{ kind: "workflow_finished" }]"#, Feature::Workflows; "workflow_finished")]
    #[test_case(r#"triggers: [#{ kind: "armed" }], workflows: ["review-changes"]"#, Feature::Workflows; "start_workflow")]
    fn a_script_needs_each_feature_it_uses(fields: &str, feature: Feature) {
        let fixture = Fixture::new();
        let path = fixture.write(
            &fixture.user_dir(),
            &file_name(NAME),
            script(NAME, fields).as_bytes(),
        );

        let off = fixture
            .scan(FeatureFlags::all().without(feature))
            .to_catalog();
        let on = fixture.scan(FeatureFlags::NONE.with(feature)).to_catalog();

        assert!(off.entries.is_empty());
        assert_eq!(
            off.invalid,
            [InvalidEntry {
                name: NAME.into(),
                scope: Scope::User,
                path,
                reason: format!("{NEEDS_FEATURE} {feature}"),
            }]
        );
        assert_eq!(names(&on), [NAME]);
        assert!(on.invalid.is_empty());
    }

    #[test_case(r#"triggers: [#{ kind: "idle" }, #{ kind: "schedule", every: "5m" }]"#, &[]; "session_triggers")]
    #[test_case(r#"triggers: [#{ kind: "message_received" }, #{ kind: "workflow_finished" }]"#, &[Feature::CrossSessionMessaging, Feature::Workflows]; "both")]
    fn a_header_needs_only_the_switches_it_uses(fields: &str, expected: &[Feature]) {
        let meta = parse_meta(&script(NAME, fields)).unwrap();

        assert_eq!(needed_features(&meta), expected);
    }

    #[test_case(r#"triggers: [#{ kind: "armed" }, #{ kind: "needs_input" }]"#, "triggers[1]: needs_input"; "needs_input")]
    #[test_case(r#"triggers: [#{ kind: "message_received" }]"#, "triggers[0]: message_received"; "message_received")]
    #[test_case(r#"triggers: [#{ kind: "work_finished" }]"#, "triggers[0]: work_finished"; "work_finished")]
    #[test_case(r#"triggers: [#{ kind: "armed" }], messaging: #{ reply: true }"#, MESSAGING_FIELD; "messaging")]
    fn an_sdk_session_refuses_what_only_the_tui_serves(fields: &str, unavailable: &str) {
        let fixture = Fixture::new();
        let path = fixture.write(
            &fixture.user_dir(),
            &file_name(NAME),
            script(NAME, fields).as_bytes(),
        );

        let sdk = fixture.scan_in(FeatureFlags::all(), Frontend::Sdk);
        let tui = fixture.scan_in(FeatureFlags::all(), Frontend::Tui);

        let refused = sdk.to_catalog();
        assert!(refused.entries.is_empty());
        assert_eq!(
            refused.invalid,
            [InvalidEntry {
                name: NAME.into(),
                scope: Scope::User,
                path,
                reason: format!("{META_VARIABLE}.{unavailable} {UNAVAILABLE_IN_SDK}"),
            }]
        );
        assert!(sdk.unavailable_here(NAME));
        assert_eq!(names(&tui.to_catalog()), [NAME]);
        assert!(!tui.unavailable_here(NAME));
    }

    #[test_case(Scope::User, Ok(vec![ALWAYS_WITH_SCHEDULE.to_owned()]); "user_scope_warns")]
    #[test_case(Scope::Project, Err(ALWAYS_OUTSIDE_USER_SCOPE.to_owned()); "project_scope_is_invalid")]
    #[test_case(Scope::Bundled, Err(ALWAYS_OUTSIDE_USER_SCOPE.to_owned()); "bundled_scope_is_invalid")]
    fn arming_always_follows_the_scope(scope: Scope, expected: Result<Vec<String>, String>) {
        let fixture = Fixture::new();
        let bundled: Vec<BundledAutomation> = fixture
            .place(scope, NAME, &script(NAME, ALWAYS_ON_SCHEDULE))
            .into_iter()
            .collect();

        let listed = fixture.scan_bundled(&bundled).to_catalog();

        let outcome = match (listed.entries.as_slice(), listed.invalid.as_slice()) {
            ([entry], []) => Ok(entry.warnings.clone()),
            ([], [invalid]) => Err(invalid.reason.clone()),
            _ => panic!("{listed:?}"),
        };
        assert_eq!(outcome, expected);
    }

    #[test]
    fn a_project_script_shadows_a_user_script_of_the_same_name() {
        let fixture = Fixture::new();
        let project_source = format!("{}// project\n", script(NAME, ARMED));
        let project_path = fixture.write(
            &fixture.project_dir(),
            &file_name(NAME),
            project_source.as_bytes(),
        );
        fixture.write(
            &fixture.user_dir(),
            &file_name(NAME),
            script(NAME, ARMED).as_bytes(),
        );

        let catalog = fixture.scan(FeatureFlags::NONE);

        let listed = catalog.to_catalog();
        let [entry] = listed.entries.as_slice() else {
            panic!("{ONE_ENTRY_PER_NAME}: {listed:?}");
        };
        assert_eq!(
            (entry.scope, &entry.path),
            (Scope::Project, &project_path),
            "{PROJECT_WINS}"
        );
        assert_eq!(entry.shadowed, [Scope::User], "{ONE_ENTRY_PER_NAME}");
        assert!(listed.invalid.is_empty());
        assert_eq!(
            catalog.resolve(NAME).unwrap().source,
            project_source,
            "{PROJECT_WINS}"
        );
    }

    #[test_case(&[Scope::Bundled] => (Scope::Bundled, Vec::new()); "bundled_alone")]
    #[test_case(&[Scope::User, Scope::Bundled] => (Scope::User, vec![Scope::Bundled]); "user_over_bundled")]
    #[test_case(&[Scope::Project, Scope::Bundled] => (Scope::Project, vec![Scope::Bundled]); "project_over_bundled")]
    #[test_case(&[Scope::Project, Scope::User, Scope::Bundled] => (Scope::Project, vec![Scope::User, Scope::Bundled]); "project_over_both")]
    fn the_nearest_scope_wins_and_records_the_hidden_ones(scopes: &[Scope]) -> (Scope, Vec<Scope>) {
        let fixture = Fixture::new();
        let source = |scope: Scope| format!("{}// {scope:?}\n", script(NAME, ARMED));
        let bundled: Vec<BundledAutomation> = scopes
            .iter()
            .filter_map(|&scope| fixture.place(scope, NAME, &source(scope)))
            .collect();

        let catalog = fixture.scan_bundled(&bundled);

        let listed = catalog.to_catalog();
        let [entry] = listed.entries.as_slice() else {
            panic!("{ONE_ENTRY_PER_NAME}: {listed:?}");
        };
        assert_eq!(catalog.resolve(NAME).unwrap().source, source(entry.scope));
        (entry.scope, entry.shadowed.clone())
    }

    #[test]
    fn a_bundled_script_is_listed_trusted_and_armable_without_a_file() {
        let fixture = Fixture::new();
        let source = script(NAME, ARMED);
        let shipped = bundled(NAME, &source);

        let catalog = fixture.scan_bundled(slice::from_ref(&shipped));
        let resolved = catalog.resolve(NAME).unwrap();

        assert_eq!(resolved.source, source);
        assert_eq!(resolved.entry.scope, Scope::Bundled);
        assert_eq!(resolved.entry.path, shipped.path());
        assert!(!resolved.entry.path.exists(), "{BUNDLED_HAS_NO_FILE}");
        assert_eq!(resolved.entry.digest, digest_of(source.as_bytes()));
        assert_eq!(
            resolved.entry.trust,
            Trust::Location,
            "{BUNDLED_IS_TRUSTED}"
        );
        assert!(matches!(
            catalog.trust(&fixture.state_dir, NAME, &resolved.entry.digest),
            Ok(())
        ));
    }

    #[test]
    fn a_bundled_script_named_unlike_its_header_is_listed_invalid() {
        let fixture = Fixture::new();
        let shipped = bundled(MISMATCHED_STEM, &script(NAME, ARMED));

        let listed = fixture.scan_bundled(slice::from_ref(&shipped)).to_catalog();

        assert!(listed.entries.is_empty());
        let [invalid] = listed.invalid.as_slice() else {
            panic!("{:?}", listed.invalid);
        };
        assert_eq!(
            (invalid.name.as_str(), invalid.scope, &invalid.path),
            (MISMATCHED_STEM, Scope::Bundled, &shipped.path())
        );
        assert!(
            invalid.reason.starts_with(STEM_MISMATCH),
            "{}",
            invalid.reason
        );
    }

    #[test]
    fn removing_a_provider_drops_its_scripts_from_the_next_scan() {
        let fixture = Fixture::new();
        let scan = || {
            let installed: Vec<BundledAutomation> = bundled_automations()
                .iter()
                .filter(|script| script.provider == REGISTRY_PROVIDER)
                .cloned()
                .collect();
            fixture
                .scan_bundled(&installed)
                .find(BUNDLED_NAME)
                .is_some()
        };
        install_bundled_automations(REGISTRY_PROVIDER, [(BUNDLED_NAME, BUNDLED_SOURCE)]);
        assert!(scan());

        remove_bundled_automations(REGISTRY_PROVIDER);

        assert!(!scan());
    }

    #[test]
    fn the_user_only_scan_reads_the_user_and_bundled_scopes() {
        let fixture = Fixture::new();
        fixture.write(
            &fixture.project_dir(),
            &file_name(NAME),
            script(NAME, ARMED).as_bytes(),
        );
        fixture.write(&fixture.project_dir(), &file_name(OTHER_NAME), NOT_FIRST);
        fixture.write(
            &fixture.user_dir(),
            &file_name(OTHER_NAME),
            script(OTHER_NAME, ARMED).as_bytes(),
        );

        let catalog = Catalog::scan_user_only(
            Some(&fixture.config),
            &[bundled(BUNDLED_NAME, BUNDLED_SOURCE)],
            FeatureFlags::NONE,
            Frontend::Tui,
        );

        let listed = catalog.to_catalog();
        assert_eq!(
            names(&listed),
            [OTHER_NAME, BUNDLED_NAME],
            "{PROJECT_IS_IGNORED}"
        );
        assert!(listed.invalid.is_empty(), "{PROJECT_IS_IGNORED}");
        assert!(matches!(
            catalog.resolve(NAME),
            Err(CatalogError::Unknown { name }) if name == NAME
        ));
    }

    #[test]
    fn missing_directories_are_empty_scopes() {
        let fixture = Fixture::new();
        let missing_cwd = fixture.project.join("absent");

        for catalog in [
            fixture.scan(FeatureFlags::NONE),
            Catalog::scan_with(
                &fixture.state_dir,
                &missing_cwd,
                None,
                &[],
                FeatureFlags::NONE,
                Frontend::Tui,
            ),
            Catalog::scan_user_only(None, &[], FeatureFlags::NONE, Frontend::Tui),
        ] {
            let listed = catalog.to_catalog();
            assert!(listed.entries.is_empty());
            assert!(listed.invalid.is_empty(), "{NOTHING_INVALID}");
        }
    }
}
