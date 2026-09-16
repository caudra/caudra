use super::{PermissionPolicyError, PermissionResourceKind, PermissionResourceSelector};
use std::path::{Path, PathBuf};

pub(super) const SUBTREE_SCOPE_SUFFIX: &str = "/**";

pub(super) const BASH_WORKDIR_SCOPE_MARKER: &str = " # caudra-workdir[";

pub(super) const BASH_WORKDIR_FRAME_MARKER: &str = " # caudra-frame[";

pub const BOUNDARY_UNVERIFIABLE_PREFIX: &str = "Cannot verify project boundary for";

pub(super) fn normalize_configured_selector(
    selector: &mut PermissionResourceSelector,
    kind: &PermissionResourceKind,
    project: &Path,
    home: Option<&Path>,
) -> Result<(), PermissionPolicyError> {
    if !matches!(
        kind,
        PermissionResourceKind::File | PermissionResourceKind::Directory
    ) {
        return Ok(());
    }
    let path = match selector {
        PermissionResourceSelector::Exact { value } => value,
        PermissionResourceSelector::Subtree { root } => root,
        _ => return Ok(()),
    };
    let expanded = if path == "~" || path.starts_with("~/") {
        let home = home.filter(|home| home.is_absolute()).ok_or_else(|| {
            PermissionPolicyError(
                "configured filesystem scope requires an absolute home directory".into(),
            )
        })?;
        home.join(path.strip_prefix("~/").unwrap_or(""))
    } else {
        PathBuf::from(&*path)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        project.join(expanded)
    };
    if !absolute.is_absolute() {
        return Err(PermissionPolicyError(
            "configured filesystem scope requires an absolute project base".into(),
        ));
    }
    *path = caudra_storage::paths::incremental_canonicalize(&absolute)
        .and_then(|path| path.to_str().map(str::to_owned))
        .ok_or_else(|| {
            PermissionPolicyError("configured filesystem scope cannot be normalized".into())
        })?;
    Ok(())
}

pub fn shell_permission_scope(command: &str, workdir: &Path) -> String {
    let workdir = workdir.to_string_lossy();
    format!(
        "{command}{BASH_WORKDIR_SCOPE_MARKER}{}]={workdir}{BASH_WORKDIR_FRAME_MARKER}{}]",
        workdir.len(),
        workdir.len()
    )
}

pub(super) fn bash_scope_parts(scope: &str) -> Option<(&str, &str)> {
    let (payload, frame) = scope.rsplit_once(BASH_WORKDIR_FRAME_MARKER)?;
    let workdir_length = frame.strip_suffix(']')?.parse::<usize>().ok()?;
    let workdir_start = payload.len().checked_sub(workdir_length)?;
    let command_with_metadata = payload.get(..workdir_start)?;
    let workdir = payload.get(workdir_start..)?;
    let metadata = format!("{BASH_WORKDIR_SCOPE_MARKER}{workdir_length}]=");
    Some((command_with_metadata.strip_suffix(&metadata)?, workdir))
}

/// Glob matcher for permission scopes. The boundary suffixes (`/**`, `" *"`)
/// must be tried before the bare `*`, otherwise a plain prefix would swallow
/// them. `" *"` is the bash form `<command> *`: it has to match the bare
/// command too (`pwd *` covers `pwd` and `pwd -L`, but not `pwdx`).
///
/// For the `/**` path pattern, `Path::starts_with` is used to compare
/// components rather than characters, which handles both `/` and `\`
/// transparently on all platforms.
pub fn scope_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        // Normalize both sides the same way: absolutize, then resolve symlinks
        // in existing leading components before appending the lexical tail.
        // Absolutizing first keeps a relative rule like `dist/**` matching
        // before the dir exists, since `incremental_canonicalize` leaves a
        // relative path relative when the leading component is missing.
        let norm = |p: &str| {
            let abs = std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p));
            caudra_storage::paths::incremental_canonicalize(&abs)
                .unwrap_or_else(|| caudra_storage::paths::normalize_path(&abs))
        };
        let norm_prefix = norm(prefix);
        let norm_value = norm(value);
        return norm_value == norm_prefix || norm_value.starts_with(&norm_prefix);
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return value == prefix || value.starts_with(&format!("{prefix} "));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

/// Lexical normalization for scope paths. Resolves `..` and `.` without
/// hitting the filesystem and without producing `\\?\` prefixes on Windows.
/// Use this for display, logging, and scope matching.
///
/// For symlink-aware security checks, use [`physical_boundary_check`].
pub fn normalize_scope_path(path: &str) -> String {
    let resolved = crate::tools::resolve_path(path).unwrap_or_else(|_| path.to_string());
    caudra_storage::paths::normalize_path(Path::new(&resolved))
        .to_string_lossy()
        .into_owned()
}

/// Check whether `child` is physically inside `parent`, following symlinks.
///
/// Uses incremental left-to-right canonicalization: each component is
/// resolved through the filesystem (including symlinks) *before* any
/// subsequent `..` component can act on it. This prevents symlink-based
/// boundary escapes where a symlink followed by `..` resolves to a
/// location outside the parent.
///
/// Returns `true` only when the resolved filesystem location of `child`
/// is under `parent`. Returns `None` if the parent itself cannot be resolved.
pub fn physical_boundary_check(parent: &Path, child: &Path) -> Option<bool> {
    let parent_canon = caudra_storage::paths::incremental_canonicalize(parent)?;
    let child_canon = caudra_storage::paths::incremental_canonicalize(child)
        .unwrap_or_else(|| child.to_path_buf());
    Some(child_canon.starts_with(&parent_canon))
}

#[cfg(test)]
mod tests {
    use super::PermissionResourceSelector;

    use test_case::test_case;

    use crate::permissions::tests::{
        SHELL_WORKDIR, coverage_with, covered_flags, decisions, make_config, mgr_with,
        shell_policy_rule, shell_request, workcell_shell_subject,
    };
    use crate::permissions::{
        NORMALIZED_COMMAND_ATTRIBUTE, PermissionRequest, PermissionResourceKind,
        StructuredPermissionDecision, configured_selector, normalize_configured_selector,
        permission_rule_intersects_request, resource_constraint_matches, scope_matches,
    };
    use caudra_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
    use std::path::{Path, PathBuf};
    /// `cmd *` is a command pattern, not a text prefix: it has always covered
    /// the bare invocation as well as the one with arguments, and it stops at a
    /// token boundary so it cannot reach a longer executable name.
    #[test]
    fn a_configured_command_pattern_covers_the_bare_invocation_only_to_its_token_boundary() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("pwd *", Effect::Allow)]),
            PathBuf::from("/tmp"),
        );
        let covered = |command: &str| {
            let request = shell_request(&[command], workcell_shell_subject());
            covered_flags(&coverage_with(&manager, &request, false, &[]))
        };

        assert_eq!(covered("pwd"), [true]);
        assert_eq!(covered("pwd -L"), [true]);
        assert_eq!(covered("pwdx 1"), [false]);
    }

    #[test]
    fn normalized_executable_names_apply_only_to_restrictive_shell_policy() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule("git status *", Effect::Allow),
                shell_policy_rule("rm *", Effect::Deny),
                shell_policy_rule("curl *", Effect::Ask),
            ]),
            PathBuf::from("/tmp"),
        );
        let request = |source: &str, normalized: &str| {
            let mut request = shell_request(&[source], workcell_shell_subject());
            request.resources[0]
                .attributes
                .insert(NORMALIZED_COMMAND_ATTRIBUTE.into(), normalized.into());
            request
        };

        assert_eq!(
            decisions(
                &manager,
                &request("/tmp/git status --short", "git status --short")
            ),
            vec![StructuredPermissionDecision::NoMatch]
        );
        assert_eq!(
            decisions(&manager, &request("/bin/rm -rf build", "rm -rf build")),
            vec![StructuredPermissionDecision::Deny]
        );
        assert_eq!(
            decisions(
                &manager,
                &request("/usr/bin/curl example.com", "curl example.com")
            ),
            vec![StructuredPermissionDecision::Ask]
        );

        let builtin_manager = mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"));
        let request = request("/bin/rm -rf build", "rm -rf build");
        let coverage = coverage_with(&builtin_manager, &request, true, &[]);
        assert!(coverage.must_prompt);
    }

    #[test_case("*", "anything" => true ; "star")]
    #[test_case("cargo *", "cargo test" => true ; "prefix")]
    #[test_case("cargo *", "git push" => false ; "prefix_no_match")]
    #[test_case("pwd *", "pwd" => true ; "space_star_matches_bare_command")]
    #[test_case("pwd *", "pwd -L" => true ; "space_star_matches_with_args")]
    #[test_case("pwd *", "pwdx" => false ; "space_star_no_partial_token")]
    #[test_case("src/**", "src/main.rs" => true ; "glob")]
    #[test_case("src/**", "src/deep/nested/file.rs" => true ; "glob_deep_nested")]
    #[test_case("src/**", "src" => true ; "glob_exact_prefix")]
    #[test_case("src/**", "srcfoo" => false ; "glob_no_bare_prefix")]
    #[test_case("src/**", "other/src/main.rs" => false ; "glob_no_inner_match")]
    fn scope_match(pattern: &str, value: &str) -> bool {
        scope_matches(pattern, value)
    }

    #[test]
    #[cfg(unix)]
    fn scope_matches_resolves_symlinked_parent() {
        let tmp = std::env::temp_dir();
        let real = tmp.join("__caudra_test_scope_symlink_real");
        let link = tmp.join("__caudra_test_scope_symlink_link");
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let pattern = format!("{}/**", real.display());
        let value = format!("{}/new_file.txt", link.display());
        assert!(
            scope_matches(&pattern, &value),
            "symlinked parent should resolve: pattern={pattern}, value={value}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn scope_matches_relative_pattern_before_dir_exists() {
        // A relative rule like `dist/**` must match an absolute value even
        // before the directory exists.
        let cwd = std::env::current_dir().unwrap();
        let value = cwd.join("__caudra_nonexistent_dist/file.txt");
        assert!(!value.exists(), "test dir must not exist");
        assert!(
            scope_matches("__caudra_nonexistent_dist/**", &value.to_string_lossy()),
            "relative pattern should match absolute value: value={}",
            value.display()
        );
    }

    #[test]
    #[cfg(unix)]
    fn scope_matches_symlinked_parent_with_nonexistent_tail() {
        // Regression: symlinked leading component plus a non-existent tail
        // (`proj`). Both sides must resolve the symlink before appending the
        // lexical tail, else the prefix stays lexical and this returns false.
        let tmp = std::env::temp_dir();
        let real = tmp.join("__caudra_test_scope_symlink_tail_real");
        let link = tmp.join("__caudra_test_scope_symlink_tail_link");
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // `proj` under the symlink does not exist.
        let pattern = format!("{}/proj/**", link.display());
        let value = format!("{}/proj/file.txt", link.display());
        assert!(
            scope_matches(&pattern, &value),
            "symlinked parent with non-existent tail should match: pattern={pattern}, value={value}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn boundary_inside_proceeds() {
        let tmp = std::env::temp_dir();
        let mgr = mgr_with(PermissionsConfig::default(), tmp.clone());
        assert!(
            mgr.boundary_block_reason(&tmp.join("some_file.txt"))
                .is_none()
        );
    }

    #[test]
    fn boundary_outside_proceeds_via_prompt() {
        let tmp = std::env::temp_dir();
        let mgr = mgr_with(PermissionsConfig::default(), tmp);
        #[cfg(unix)]
        let outside = Path::new("/etc/hosts");
        #[cfg(windows)]
        let outside = Path::new(r"C:\Windows\System32\drivers\etc\hosts");
        assert!(mgr.boundary_block_reason(outside).is_none());
    }

    #[test]
    fn boundary_dotdot_smuggling_proceeds_via_prompt() {
        let tmp = std::env::temp_dir();
        let sub = tmp.join("__caudra_test_boundary");
        std::fs::create_dir_all(&sub).unwrap();
        #[cfg(unix)]
        let attack = sub
            .join("x")
            .join("..")
            .join("..")
            .join("..")
            .join("etc")
            .join("passwd");
        #[cfg(windows)]
        let attack = sub
            .join("x")
            .join("..")
            .join("..")
            .join("..")
            .join("Windows")
            .join("System32");
        let mgr = mgr_with(PermissionsConfig::default(), sub.clone());
        assert!(
            mgr.boundary_block_reason(&attack).is_none(),
            "outside-cwd dotdot path should prompt, not hard-block: {}",
            attack.display()
        );
        let _ = std::fs::remove_dir_all(&sub);
    }

    #[test]
    #[cfg(unix)]
    fn boundary_symlink_escape_proceeds_via_prompt() {
        // Lexical normalization resolves this inside (/project/escape), but
        // incremental canonicalization follows the symlink first, so `..`
        // escapes outside. The permission prompt catches it, not this function.
        let tmp = std::env::temp_dir();
        let project = tmp.join("__caudra_test_symlink_escape");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        let link = project.join("link");
        let _ = std::os::unix::fs::symlink(&tmp, &link);

        let attack = link.join("..").join("escape_target");
        let mgr = mgr_with(PermissionsConfig::default(), project.clone());
        assert!(
            mgr.boundary_block_reason(&attack).is_none(),
            "outside-boundary edits are gated by the prompt, not hard-blocked: {}",
            attack.display()
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn boundary_nonexistent_cwd_proceeds_via_lexical_tail() {
        let missing = std::env::temp_dir().join("__caudra_test_absent_cwd_xyz");
        let _ = std::fs::remove_dir_all(&missing);
        let mgr = mgr_with(PermissionsConfig::default(), missing.clone());
        assert!(
            mgr.boundary_block_reason(&missing.join("file.txt"))
                .is_none()
        );
    }

    #[test_case(Effect::Allow; "allow")]
    #[test_case(Effect::Deny; "deny")]
    fn configured_relative_filesystem_scopes_use_the_explicit_project(effect: Effect) {
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let manager = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::native("file_read"),
                scope: Some("missing/../future/**".into()),
                effect,
            }]),
            project.path().to_path_buf(),
        );
        let path = project.path().join("future/file.txt");
        let request = PermissionRequest::from_legacy(
            "configured".into(),
            ToolKey::native("file_read"),
            vec![path.display().to_string()],
            serde_json::json!({"filePath": path}),
            project.path(),
            false,
        );
        let rules = manager
            .configured_structured_rules(&request, false)
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert!(resource_constraint_matches(
            &rules[0].rule.resources[0],
            &request.resources[0]
        ));
        manager.set_project(other.path());
        let rules = manager
            .configured_structured_rules(&request, false)
            .unwrap();
        assert!(!resource_constraint_matches(
            &rules[0].rule.resources[0],
            &request.resources[0]
        ));
    }

    #[test_case("~"; "home_exact")]
    #[test_case("~/future/**"; "home_subtree")]
    fn configured_home_resolution_fails_closed(scope: &str) {
        let mut selector = configured_selector(scope, &PermissionResourceKind::File);
        let error = normalize_configured_selector(
            &mut selector,
            &PermissionResourceKind::File,
            Path::new(SHELL_WORKDIR),
            None,
        )
        .unwrap_err();
        const HOME_ERROR: &str = "configured filesystem scope requires an absolute home directory";
        assert_eq!(error.0, HOME_ERROR);
    }

    #[test_case("~/future/**", "future/file"; "subtree")]
    #[test_case("~/future/file", "future/file"; "exact")]
    fn active_configured_policy_expands_only_the_rule_home(scope: &str, relative: &str) {
        let Some(home) = caudra_storage::paths::home() else {
            return;
        };
        let project = tempfile::tempdir().unwrap();
        let manager = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::native("file_read"),
                scope: Some(scope.into()),
                effect: Effect::Deny,
            }]),
            project.path().to_path_buf(),
        );
        let path = home.join(relative);
        let request = PermissionRequest::from_legacy(
            "configured-home".into(),
            ToolKey::native("file_read"),
            vec![path.display().to_string()],
            serde_json::json!({"filePath": path}),
            project.path(),
            false,
        );
        let before = request.input_digest.clone();
        let rules = manager
            .configured_structured_rules(&request, false)
            .unwrap();
        assert!(permission_rule_intersects_request(&rules[0].rule, &request));
        assert_eq!(request.input_digest, before);
    }
    #[test_case("nested/~/file", "nested/~/file"; "embedded_home_is_literal")]
    #[test_case("~other/file", "~other/file"; "named_home_is_literal")]
    fn configured_paths_preserve_literal_tildes(scope: &str, relative: &str) {
        let project = tempfile::tempdir().unwrap();
        let mut selector = configured_selector(scope, &PermissionResourceKind::File);
        normalize_configured_selector(
            &mut selector,
            &PermissionResourceKind::File,
            project.path(),
            None,
        )
        .unwrap();
        assert_eq!(
            selector,
            PermissionResourceSelector::Exact {
                value: project.path().join(relative).display().to_string()
            }
        );
    }

    #[test_case("~/future*"; "home_prefix_is_literal")]
    #[test_case("relative*"; "relative_prefix_is_literal")]
    fn configured_prefix_keeps_generic_text_semantics(scope: &str) {
        let mut selector = configured_selector(scope, &PermissionResourceKind::File);
        let original = selector.clone();
        normalize_configured_selector(
            &mut selector,
            &PermissionResourceKind::File,
            Path::new("relative-base"),
            None,
        )
        .unwrap();
        assert_eq!(selector, original);
    }

    #[cfg(unix)]
    #[test_case(Effect::Allow; "allow")]
    #[test_case(Effect::Deny; "deny")]
    fn active_configured_paths_follow_symlinks_before_parent_components(effect: Effect) {
        use std::os::unix::fs::symlink;
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("nested")).unwrap();
        symlink(outside.path().join("nested"), project.path().join("alias")).unwrap();
        let manager = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::native("file_read"),
                scope: Some("alias/../future/**".into()),
                effect,
            }]),
            project.path().to_path_buf(),
        );
        let path = outside.path().join("future/file");
        let request = PermissionRequest::from_legacy(
            "symlink-rule".into(),
            ToolKey::native("file_read"),
            vec![path.display().to_string()],
            serde_json::json!({"filePath": path}),
            project.path(),
            false,
        );
        let rules = manager
            .configured_structured_rules(&request, false)
            .unwrap();
        assert!(resource_constraint_matches(
            &rules[0].rule.resources[0],
            &request.resources[0]
        ));
        let mut lexical = request.resources[0].clone();
        lexical.value = project.path().join("future/file").display().to_string();
        assert!(!resource_constraint_matches(
            &rules[0].rule.resources[0],
            &lexical
        ));
    }
}
