//! Discovers the workflow scripts a session may launch: compiled-in
//! built-ins, then `<project>/.caudra/workflows`, then the user's
//! `<config>/workflows`. The first scope to declare a name wins; a built-in
//! name can never be taken over by a file on disk.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use caudra_storage::StateDir;
use caudra_storage::paths::{config_dir, user_config_dir};
use caudra_storage::workflow_source::read_bounded_regular_file;
use caudra_storage::workflow_trust::{is_workflow_trusted, trust_workflow, workflow_source_digest};
use caudra_workflow::meta::MAX_SOURCE_BYTES;
use caudra_workflow::{
    CatalogEntry, DEEP_RESEARCH_NAME, DEEP_RESEARCH_SOURCE, InvalidEntry, SourceKind,
    WORKFLOW_ABI_VERSION, WORKFLOW_LANGUAGE_VERSION, WorkflowCatalog, WorkflowError, WorkflowMeta,
    parse_meta,
};
use tracing::{debug, warn};

const PROJECT_DIR: &str = ".caudra";
const WORKFLOWS_SUBDIR: &str = "workflows";
const SCRIPT_EXTENSION: &str = "rhai";
const BUILTINS: [(&str, &str); 1] = [(DEEP_RESEARCH_NAME, DEEP_RESEARCH_SOURCE)];
const BUILTIN_PATH_PREFIX: &str = "<builtin>";
const NOT_UTF8: &str = "source is not valid UTF-8";
const NOT_A_DIRECTORY: &str = "is not a real directory";
const SHADOWS_BUILTIN: &str = "shadows a built-in workflow";

/// A script ready to launch. `source` is the exact text that was hashed
/// into `digest`, so what the user trusted is what runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkflow {
    pub meta: WorkflowMeta,
    pub source: String,
    pub digest: String,
    pub source_kind: SourceKind,
    pub path: Option<PathBuf>,
    pub trusted: bool,
}

struct Discovered {
    resolved: ResolvedWorkflow,
    shadowed: Vec<SourceKind>,
}

enum Claim {
    Winner(usize),
    Ambiguous(SourceKind),
}

pub struct Catalog {
    entries: Vec<Discovered>,
    invalid: Vec<InvalidEntry>,
    claims: BTreeMap<String, Claim>,
    project_root: Option<PathBuf>,
    user_dir: Option<PathBuf>,
}

impl Catalog {
    pub fn scan(state_dir: &StateDir, cwd: &Path) -> Self {
        Self::scan_with(state_dir, cwd, config_dir().ok().as_deref())
    }

    /// `user_config` is the user's config directory; production passes
    /// [`config_dir`], tests pass a tempdir.
    pub fn scan_with(state_dir: &StateDir, cwd: &Path, user_config: Option<&Path>) -> Self {
        let project_root = cwd
            .canonicalize()
            .map_err(|error| debug!(cwd = %cwd.display(), %error, "no project workflow scope"))
            .ok();
        let mut catalog = Self {
            entries: Vec::new(),
            invalid: Vec::new(),
            claims: BTreeMap::new(),
            project_root,
            user_dir: user_config_dir(user_config, WORKFLOWS_SUBDIR),
        };
        let builtins = builtin_candidates(&mut catalog.invalid);
        catalog.admit(SourceKind::Builtin, builtins);
        if let Some(root) = catalog.project_root.clone() {
            let dir = root.join(PROJECT_DIR).join(WORKFLOWS_SUBDIR);
            let candidates = scan_directory(
                &dir,
                SourceKind::Project,
                &mut catalog.invalid,
                |path, digest| project_trust(state_dir, &root, path, digest),
            );
            catalog.admit(SourceKind::Project, candidates);
        }
        if let Some(dir) = catalog.user_dir.clone() {
            let candidates =
                scan_directory(&dir, SourceKind::User, &mut catalog.invalid, |_, _| true);
            catalog.admit(SourceKind::User, candidates);
        }
        catalog
    }

    /// Creates the user scope directory so there is a folder to drop scripts
    /// into. Failure leaves the scope empty until the next scan can read it.
    pub fn ensure_user_scope(user_config: Option<&Path>) {
        if let Some(dir) = user_config_dir(user_config, WORKFLOWS_SUBDIR)
            && let Err(error) = fs::create_dir_all(&dir)
        {
            warn!(dir = %dir.display(), %error, "cannot create the user workflows directory");
        }
    }

    pub fn to_catalog(&self) -> WorkflowCatalog {
        WorkflowCatalog {
            entries: self
                .entries
                .iter()
                .map(|entry| {
                    let resolved = &entry.resolved;
                    CatalogEntry {
                        name: resolved.meta.name.clone(),
                        description: resolved.meta.description.clone(),
                        when_to_use: resolved.meta.when_to_use.clone(),
                        phases: resolved
                            .meta
                            .phases
                            .iter()
                            .map(|phase| phase.title.clone())
                            .collect(),
                        source_kind: resolved.source_kind,
                        path: resolved.path.clone(),
                        digest: resolved.digest.clone(),
                        trusted: resolved.trusted,
                        shadowed: entry.shadowed.clone(),
                    }
                })
                .collect(),
            invalid: self.invalid.clone(),
            project_dir: self
                .project_root
                .as_ref()
                .map(|root| root.join(PROJECT_DIR).join(WORKFLOWS_SUBDIR)),
            user_dir: self.user_dir.clone(),
        }
    }

    pub fn resolve(&self, name: &str) -> Result<ResolvedWorkflow, WorkflowError> {
        self.lookup(name).cloned()
    }

    /// Records trust for a project script, provided the caller approved the
    /// bytes currently on disk. Built-in and user scripts are trusted by
    /// where they come from, so this is a no-op for them.
    pub fn trust(
        &self,
        state_dir: &StateDir,
        name: &str,
        expected_digest: &str,
    ) -> Result<(), WorkflowError> {
        let entry = self.lookup(name)?;
        let (Some(root), Some(path)) = (&self.project_root, &entry.path) else {
            return Ok(());
        };
        if entry.source_kind != SourceKind::Project {
            return Ok(());
        }
        if entry.digest != expected_digest {
            return Err(WorkflowError::TrustRequired {
                name: name.to_owned(),
                digest: entry.digest.clone(),
                path: path.clone(),
            });
        }
        let relative = project_relative(root, path).ok_or_else(|| {
            WorkflowError::Internal(format!("{} is outside the project root", path.display()))
        })?;
        trust_workflow(state_dir, root, relative, &entry.digest)
            .map_err(|error| WorkflowError::Storage(error.to_string()))
    }

    fn lookup(&self, name: &str) -> Result<&ResolvedWorkflow, WorkflowError> {
        match self.claims.get(name) {
            Some(Claim::Winner(index)) => Ok(&self.entries[*index].resolved),
            Some(Claim::Ambiguous(scope)) => Err(WorkflowError::Ambiguous {
                name: name.to_owned(),
                scopes: vec![*scope],
            }),
            None => Err(WorkflowError::UnknownWorkflow {
                name: name.to_owned(),
            }),
        }
    }

    /// Merges one scope's parsed scripts under the precedence rules. A name
    /// two files of the scope both declare is unusable from that scope on.
    fn admit(&mut self, scope: SourceKind, candidates: Vec<ResolvedWorkflow>) {
        let mut declared: BTreeMap<&str, usize> = BTreeMap::new();
        for candidate in &candidates {
            *declared.entry(candidate.meta.name.as_str()).or_default() += 1;
        }
        let ambiguous: Vec<String> = declared
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| name.to_owned())
            .collect();
        for candidate in candidates {
            let name = candidate.meta.name.clone();
            if ambiguous.contains(&name) {
                self.claims
                    .entry(name.clone())
                    .or_insert(Claim::Ambiguous(scope));
                let error = WorkflowError::Ambiguous {
                    name,
                    scopes: vec![scope],
                };
                reject(&mut self.invalid, candidate, error.to_string());
                continue;
            }
            match self.claims.get(&name) {
                Some(Claim::Winner(index)) => {
                    let winner = &mut self.entries[*index];
                    if winner.resolved.source_kind == SourceKind::Builtin {
                        reject(&mut self.invalid, candidate, SHADOWS_BUILTIN.to_owned());
                    } else {
                        winner.shadowed.push(scope);
                    }
                }
                Some(Claim::Ambiguous(earlier)) => {
                    let error = WorkflowError::Ambiguous {
                        name,
                        scopes: vec![*earlier],
                    };
                    reject(&mut self.invalid, candidate, error.to_string());
                }
                None => {
                    self.claims.insert(name, Claim::Winner(self.entries.len()));
                    self.entries.push(Discovered {
                        resolved: candidate,
                        shadowed: Vec::new(),
                    });
                }
            }
        }
    }
}

fn reject(invalid: &mut Vec<InvalidEntry>, candidate: ResolvedWorkflow, error: String) {
    invalid.push(InvalidEntry {
        path: candidate
            .path
            .unwrap_or_else(|| builtin_path(&candidate.meta.name)),
        source_kind: candidate.source_kind,
        error,
    });
}

fn builtin_path(name: &str) -> PathBuf {
    Path::new(BUILTIN_PATH_PREFIX).join(format!("{name}.{SCRIPT_EXTENSION}"))
}

fn builtin_candidates(invalid: &mut Vec<InvalidEntry>) -> Vec<ResolvedWorkflow> {
    BUILTINS
        .into_iter()
        .filter_map(|(name, source)| {
            let parsed = parse_meta(source)
                .map_err(|error| error.to_string())
                .and_then(|meta| {
                    if meta.name == name {
                        Ok(meta)
                    } else {
                        Err(format!(
                            "meta.name {:?} does not match the built-in name {name:?}",
                            meta.name
                        ))
                    }
                });
            match parsed {
                Ok(meta) => Some(ResolvedWorkflow {
                    meta,
                    source: source.to_owned(),
                    digest: digest_of(source),
                    source_kind: SourceKind::Builtin,
                    path: None,
                    trusted: true,
                }),
                Err(error) => {
                    invalid.push(InvalidEntry {
                        path: builtin_path(name),
                        source_kind: SourceKind::Builtin,
                        error,
                    });
                    None
                }
            }
        })
        .collect()
}

fn scan_directory(
    dir: &Path,
    scope: SourceKind,
    invalid: &mut Vec<InvalidEntry>,
    trusted: impl Fn(&Path, &str) -> bool,
) -> Vec<ResolvedWorkflow> {
    let paths = match script_paths(dir) {
        Ok(paths) => paths,
        Err(error) => {
            invalid.push(InvalidEntry {
                path: dir.to_path_buf(),
                source_kind: scope,
                error,
            });
            return Vec::new();
        }
    };
    paths
        .into_iter()
        .filter_map(|path| match load_script(&path) {
            Ok((meta, source, digest)) => Some(ResolvedWorkflow {
                trusted: trusted(&path, &digest),
                meta,
                source,
                digest,
                source_kind: scope,
                path: Some(path),
            }),
            Err(error) => {
                invalid.push(InvalidEntry {
                    path,
                    source_kind: scope,
                    error,
                });
                None
            }
        })
        .collect()
}

/// The `.rhai` files directly inside `dir`, sorted by name. A missing
/// directory is an empty scope; anything else that is not a real directory
/// is an error.
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

/// Reads the file once; the same buffer is hashed, parsed, and kept as the
/// launchable source.
fn load_script(path: &Path) -> Result<(WorkflowMeta, String, String), String> {
    let bytes =
        read_bounded_regular_file(path, MAX_SOURCE_BYTES).map_err(|error| error.to_string())?;
    let source = String::from_utf8(bytes).map_err(|_| NOT_UTF8.to_owned())?;
    let digest = digest_of(&source);
    let meta = parse_meta(&source).map_err(|error| error.to_string())?;
    let expected = format!("{}.{SCRIPT_EXTENSION}", meta.name);
    if path
        .file_name()
        .is_none_or(|name| name != expected.as_str())
    {
        return Err(format!(
            "file name must be {expected:?} to match meta.name {:?}",
            meta.name
        ));
    }
    Ok((meta, source, digest))
}

fn digest_of(source: &str) -> String {
    workflow_source_digest(
        source.as_bytes(),
        WORKFLOW_LANGUAGE_VERSION,
        WORKFLOW_ABI_VERSION,
    )
}

fn project_relative<'a>(root: &Path, path: &'a Path) -> Option<&'a str> {
    path.strip_prefix(root).ok()?.to_str()
}

fn project_trust(state_dir: &StateDir, root: &Path, path: &Path, digest: &str) -> bool {
    let Some(relative) = project_relative(root, path) else {
        return false;
    };
    is_workflow_trusted(state_dir, root, relative, digest).unwrap_or_else(|error| {
        warn!(path = %path.display(), %error, "workflow trust lookup failed");
        false
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const REVIEW: &str = "review";
    const DESCRIPTION: &str = "Reviews a branch";
    const BUILTIN_IS_TRUSTED: &str = "a compiled-in workflow needs no approval";
    const TRUST_IS_EXACT: &str = "trust must follow the approved bytes exactly";
    const PROJECT_WINS: &str = "the project scope must take precedence over the user scope";
    const BUILTIN_WINS: &str = "a built-in name must never resolve to a file on disk";
    const BOTH_REJECTED: &str = "every file declaring an ambiguous name must be rejected";
    const NOTHING_INVALID: &str = "missing directories must not be reported as errors";

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

        fn project_workflows(&self) -> PathBuf {
            self.project.join(PROJECT_DIR).join(WORKFLOWS_SUBDIR)
        }

        fn user_workflows(&self) -> PathBuf {
            self.config.join(WORKFLOWS_SUBDIR)
        }

        fn write(&self, dir: &Path, file: &str, content: &[u8]) -> PathBuf {
            fs::create_dir_all(dir).unwrap();
            let path = dir.join(file);
            fs::write(&path, content).unwrap();
            path
        }

        fn scan(&self) -> Catalog {
            Catalog::scan_with(&self.state_dir, &self.project, Some(&self.config))
        }
    }

    fn script(name: &str) -> String {
        format!("let meta = #{{ name: \"{name}\", description: \"{DESCRIPTION}\" }};\n")
    }

    fn file_name(name: &str) -> String {
        format!("{name}.{SCRIPT_EXTENSION}")
    }

    fn names(catalog: &WorkflowCatalog) -> Vec<&str> {
        catalog
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    #[test]
    fn the_user_scope_directory_is_created_on_demand() {
        let fixture = Fixture::new();

        Catalog::ensure_user_scope(Some(&fixture.config));

        assert!(fixture.user_workflows().is_dir());
        assert!(
            fixture.scan().to_catalog().invalid.is_empty(),
            "{NOTHING_INVALID}"
        );
    }

    #[test]
    fn builtin_is_present_and_trusted() {
        let fixture = Fixture::new();

        let catalog = fixture.scan();
        let resolved = catalog.resolve(DEEP_RESEARCH_NAME).unwrap();

        let listed = catalog.to_catalog();
        assert_eq!(names(&listed), [DEEP_RESEARCH_NAME]);
        assert!(listed.invalid.is_empty(), "{NOTHING_INVALID}");
        assert_eq!(resolved.source_kind, SourceKind::Builtin);
        assert_eq!(resolved.path, None);
        assert!(resolved.trusted, "{BUILTIN_IS_TRUSTED}");
        assert_eq!(resolved.source, DEEP_RESEARCH_SOURCE);
        assert_eq!(resolved.digest, digest_of(DEEP_RESEARCH_SOURCE));
        assert_eq!(
            catalog.resolve(REVIEW),
            Err(WorkflowError::UnknownWorkflow {
                name: REVIEW.into()
            })
        );
    }

    #[test]
    fn project_file_is_trusted_by_exact_digest() {
        let fixture = Fixture::new();
        let source = script(REVIEW);
        let path = fixture.write(
            &fixture.project_workflows(),
            &file_name(REVIEW),
            source.as_bytes(),
        );
        let digest = digest_of(&source);

        let catalog = fixture.scan();
        let untrusted = catalog.resolve(REVIEW).unwrap();

        assert_eq!(untrusted.source_kind, SourceKind::Project);
        assert_eq!(untrusted.path.as_deref(), Some(path.as_path()));
        assert_eq!(untrusted.digest, digest);
        assert!(!untrusted.trusted);
        assert_eq!(
            catalog.trust(&fixture.state_dir, REVIEW, "stale"),
            Err(WorkflowError::TrustRequired {
                name: REVIEW.into(),
                digest: digest.clone(),
                path: path.clone(),
            }),
            "{TRUST_IS_EXACT}"
        );
        assert!(!fixture.scan().resolve(REVIEW).unwrap().trusted);

        catalog.trust(&fixture.state_dir, REVIEW, &digest).unwrap();
        assert!(fixture.scan().resolve(REVIEW).unwrap().trusted);

        fs::write(&path, format!("{source} ")).unwrap();
        assert!(
            !fixture.scan().resolve(REVIEW).unwrap().trusted,
            "{TRUST_IS_EXACT}"
        );
    }

    #[test]
    fn trusting_a_builtin_is_a_no_op() {
        let fixture = Fixture::new();

        let catalog = fixture.scan();

        assert_eq!(
            catalog.trust(&fixture.state_dir, DEEP_RESEARCH_NAME, "anything"),
            Ok(())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_script_is_invalid() {
        let fixture = Fixture::new();
        let target = fixture.write(&fixture.config, "elsewhere.rhai", script(REVIEW).as_bytes());
        let dir = fixture.project_workflows();
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join(file_name(REVIEW));
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let catalog = fixture.scan().to_catalog();

        assert_eq!(names(&catalog), [DEEP_RESEARCH_NAME]);
        assert_eq!(catalog.invalid.len(), 1);
        assert_eq!(catalog.invalid[0].path, link);
        assert_eq!(catalog.invalid[0].source_kind, SourceKind::Project);
        assert!(catalog.invalid[0].error.contains("symbolic link"));
    }

    #[test_case("other.rhai", script(REVIEW).into_bytes(), "meta.name"; "filename_mismatch")]
    #[test_case("review.rhai", b"let meta = #{ name: \"review\" }".to_vec(), "description"; "missing_description")]
    #[test_case("review.rhai", b"let x = 1;".to_vec(), "first statement"; "meta_not_first")]
    #[test_case("review.rhai", vec![0xff, 0xfe], NOT_UTF8; "invalid_utf8")]
    fn broken_scripts_are_invalid(file: &str, content: Vec<u8>, expected: &str) {
        let fixture = Fixture::new();
        let path = fixture.write(&fixture.user_workflows(), file, &content);

        let catalog = fixture.scan().to_catalog();

        assert_eq!(names(&catalog), [DEEP_RESEARCH_NAME]);
        assert_eq!(catalog.invalid.len(), 1);
        assert_eq!(catalog.invalid[0].path, path);
        assert_eq!(catalog.invalid[0].source_kind, SourceKind::User);
        assert!(
            catalog.invalid[0].error.contains(expected),
            "{}",
            catalog.invalid[0].error
        );
    }

    #[test]
    fn a_name_declared_twice_in_one_scope_is_ambiguous() {
        let fixture = Fixture::new();
        let mut catalog = fixture.scan();
        let candidate = |file: &str| ResolvedWorkflow {
            meta: parse_meta(&script(REVIEW)).unwrap(),
            source: script(REVIEW),
            digest: digest_of(&script(REVIEW)),
            source_kind: SourceKind::Project,
            path: Some(fixture.project_workflows().join(file)),
            trusted: false,
        };

        catalog.admit(
            SourceKind::Project,
            vec![candidate("review.rhai"), candidate("Review.rhai")],
        );
        catalog.admit(SourceKind::User, vec![candidate("review.rhai")]);

        let listed = catalog.to_catalog();
        assert_eq!(names(&listed), [DEEP_RESEARCH_NAME]);
        assert_eq!(listed.invalid.len(), 3, "{BOTH_REJECTED}");
        assert_eq!(
            catalog.resolve(REVIEW),
            Err(WorkflowError::Ambiguous {
                name: REVIEW.into(),
                scopes: vec![SourceKind::Project],
            })
        );
    }

    #[test]
    fn project_shadows_user_by_name() {
        let fixture = Fixture::new();
        let project_source = format!("{}// project\n", script(REVIEW));
        fixture.write(
            &fixture.project_workflows(),
            &file_name(REVIEW),
            project_source.as_bytes(),
        );
        fixture.write(
            &fixture.user_workflows(),
            &file_name(REVIEW),
            script(REVIEW).as_bytes(),
        );
        fixture.write(
            &fixture.user_workflows(),
            &file_name("audit"),
            script("audit").as_bytes(),
        );

        let catalog = fixture.scan();

        let listed = catalog.to_catalog();
        assert_eq!(names(&listed), [DEEP_RESEARCH_NAME, REVIEW, "audit"]);
        assert!(listed.invalid.is_empty());
        let review = &listed.entries[1];
        assert_eq!(review.source_kind, SourceKind::Project, "{PROJECT_WINS}");
        assert_eq!(review.shadowed, [SourceKind::User]);
        assert_eq!(listed.entries[2].shadowed, []);
        assert_eq!(
            catalog.resolve(REVIEW).unwrap().source,
            project_source,
            "{PROJECT_WINS}"
        );
        assert!(catalog.resolve("audit").unwrap().trusted);
    }

    #[test_case(SourceKind::Project; "project")]
    #[test_case(SourceKind::User; "user")]
    fn a_builtin_name_cannot_be_shadowed(scope: SourceKind) {
        let fixture = Fixture::new();
        let dir = match scope {
            SourceKind::Project => fixture.project_workflows(),
            SourceKind::User => fixture.user_workflows(),
            SourceKind::Builtin => unreachable!(),
        };
        let path = fixture.write(
            &dir,
            &file_name(DEEP_RESEARCH_NAME),
            script(DEEP_RESEARCH_NAME).as_bytes(),
        );

        let catalog = fixture.scan();

        let listed = catalog.to_catalog();
        assert_eq!(names(&listed), [DEEP_RESEARCH_NAME]);
        assert_eq!(listed.entries[0].shadowed, []);
        assert_eq!(
            listed.invalid,
            [InvalidEntry {
                path,
                source_kind: scope,
                error: SHADOWS_BUILTIN.into(),
            }]
        );
        assert_eq!(
            catalog.resolve(DEEP_RESEARCH_NAME).unwrap().source,
            DEEP_RESEARCH_SOURCE,
            "{BUILTIN_WINS}"
        );
    }

    #[test]
    fn missing_directories_are_empty_scopes() {
        let fixture = Fixture::new();
        let missing_cwd = fixture.project.join("absent");

        let with_dirs = fixture.scan().to_catalog();
        let without_config = Catalog::scan_with(&fixture.state_dir, &fixture.project, None);
        let without_cwd =
            Catalog::scan_with(&fixture.state_dir, &missing_cwd, Some(&fixture.config));

        for catalog in [
            with_dirs,
            without_config.to_catalog(),
            without_cwd.to_catalog(),
        ] {
            assert_eq!(names(&catalog), [DEEP_RESEARCH_NAME]);
            assert!(catalog.invalid.is_empty(), "{NOTHING_INVALID}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_workflow_directory_is_invalid() {
        let fixture = Fixture::new();
        fixture.write(&fixture.config, "real.rhai", script(REVIEW).as_bytes());
        fs::create_dir_all(fixture.project.join(PROJECT_DIR)).unwrap();
        let dir = fixture.project_workflows();
        std::os::unix::fs::symlink(&fixture.config, &dir).unwrap();

        let catalog = fixture.scan().to_catalog();

        assert_eq!(names(&catalog), [DEEP_RESEARCH_NAME]);
        assert_eq!(
            catalog.invalid,
            [InvalidEntry {
                path: dir,
                source_kind: SourceKind::Project,
                error: NOT_A_DIRECTORY.into(),
            }]
        );
    }
}
