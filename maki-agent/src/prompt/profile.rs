use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;

pub const BUILTIN_PROFILE_NAME: &str = "builtin";

const PROFILE_DIR: &str = "system-prompts";
const MAX_PROFILE_BYTES: usize = 64 * 1024;
const MAX_PROFILE_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptProfileLayout {
    #[default]
    Overlay,
    Custom,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Frontmatter {
    description: Option<String>,
    layout: PromptProfileLayout,
}

#[derive(Debug, Clone)]
pub struct SystemPromptProfile {
    name: Arc<str>,
    description: Option<Arc<str>>,
    layout: PromptProfileLayout,
    body: Arc<str>,
    path: Arc<Path>,
}

impl SystemPromptProfile {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn layout(&self) -> PromptProfileLayout {
        self.layout
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptProfileCatalog {
    profiles: BTreeMap<Arc<str>, Arc<SystemPromptProfile>>,
    invalid: BTreeMap<Arc<str>, Arc<str>>,
}

impl PromptProfileCatalog {
    pub fn discover_user() -> Self {
        Self::discover_with(
            maki_storage::paths::home().as_deref(),
            maki_storage::paths::config_dir().ok().as_deref(),
        )
    }

    fn discover_with(home: Option<&Path>, config_dir: Option<&Path>) -> Self {
        let mut catalog = Self::default();
        for dir in maki_storage::paths::user_config_dirs(home, config_dir, PROFILE_DIR) {
            catalog.load_dir(&dir);
        }
        catalog
    }

    pub fn profiles(&self) -> impl Iterator<Item = &SystemPromptProfile> {
        self.profiles.values().map(AsRef::as_ref)
    }

    pub fn get(&self, name: &str) -> Option<Arc<SystemPromptProfile>> {
        self.profiles.get(name).cloned()
    }

    pub fn resolve(
        &self,
        name: Option<&str>,
    ) -> Result<Option<Arc<SystemPromptProfile>>, PromptProfileSelectionError> {
        let Some(name) = name.filter(|name| *name != BUILTIN_PROFILE_NAME) else {
            return Ok(None);
        };
        validate_profile_name(name).map_err(|_| PromptProfileSelectionError::InvalidName {
            name: name.to_owned(),
        })?;
        if let Some(profile) = self.get(name) {
            return Ok(Some(profile));
        }
        if let Some(reason) = self.invalid.get(name) {
            return Err(PromptProfileSelectionError::InvalidProfile {
                name: name.to_owned(),
                reason: reason.to_string(),
            });
        }
        Err(PromptProfileSelectionError::NotFound {
            name: name.to_owned(),
            available: self
                .profiles
                .keys()
                .map(|name| name.as_ref())
                .collect::<Vec<_>>()
                .join(", "),
        })
    }

    fn load_dir(&mut self, dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
            .filter(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
            .collect::<Vec<_>>();
        paths.sort();

        for path in paths {
            let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            if self.profiles.contains_key(name) || self.invalid.contains_key(name) {
                continue;
            }
            let name: Arc<str> = Arc::from(name);
            match load_profile(&path, Arc::clone(&name)) {
                Ok(profile) => {
                    self.profiles.insert(name, Arc::new(profile));
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "ignoring invalid system prompt profile");
                    self.invalid.insert(name, Arc::from(error.to_string()));
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum PromptProfileSelectionError {
    #[error(
        "invalid system prompt profile name {name:?}; expected 1-64 ASCII letters, digits, '-' or '_'"
    )]
    InvalidName { name: String },
    #[error("system prompt profile {name:?} is invalid: {reason}")]
    InvalidProfile { name: String, reason: String },
    #[error("system prompt profile {name:?} not found{suffix}", suffix = available_suffix(.available))]
    NotFound { name: String, available: String },
}

fn available_suffix(available: &str) -> String {
    if available.is_empty() {
        "; no user profiles were found".to_owned()
    } else {
        format!("; available profiles: {available}")
    }
}

#[derive(Debug, Error)]
enum PromptProfileError {
    #[error("invalid profile name")]
    InvalidName,
    #[error("the profile name 'builtin' is reserved")]
    ReservedName,
    #[error("cannot read profile: {0}")]
    Read(#[from] std::io::Error),
    #[error("profile exceeds the {MAX_PROFILE_BYTES}-byte limit")]
    TooLarge,
    #[error("profile is not valid UTF-8")]
    InvalidUtf8,
    #[error("frontmatter is not closed with '---'")]
    UnclosedFrontmatter,
    #[error("invalid frontmatter: {0}")]
    InvalidFrontmatter(#[from] serde_yaml::Error),
    #[error("profile body is empty")]
    Empty,
    #[error("unknown template directive {directive:?} on line {line}")]
    UnknownDirective { directive: String, line: usize },
    #[error("template directive {directive:?} is repeated on line {line}")]
    DuplicateDirective { directive: String, line: usize },
    #[error("{{{{maki.default}}}} cannot be combined with component directives")]
    DefaultMixedWithComponents,
}

fn validate_profile_name(name: &str) -> Result<(), PromptProfileError> {
    if name == BUILTIN_PROFILE_NAME {
        return Err(PromptProfileError::ReservedName);
    }
    if name.is_empty()
        || name.len() > MAX_PROFILE_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(PromptProfileError::InvalidName);
    }
    Ok(())
}

fn load_profile(path: &Path, name: Arc<str>) -> Result<SystemPromptProfile, PromptProfileError> {
    validate_profile_name(&name)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(PromptProfileError::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| PromptProfileError::InvalidUtf8)?;
    let (frontmatter, body, mut body_line) =
        parse_frontmatter(text.strip_prefix('\u{feff}').unwrap_or(&text))?;
    let trimmed = body.trim_start_matches(['\r', '\n']);
    body_line += body[..body.len() - trimmed.len()]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let body = trimmed.trim_end_matches(['\r', '\n']);
    if body.trim().is_empty() {
        return Err(PromptProfileError::Empty);
    }
    if frontmatter.layout == PromptProfileLayout::Custom {
        validate_custom_template(body, body_line)?;
    }
    let description = frontmatter
        .description
        .map(|description| Arc::from(description.trim()))
        .filter(|description: &Arc<str>| !description.is_empty());
    Ok(SystemPromptProfile {
        name,
        description,
        layout: frontmatter.layout,
        body: Arc::from(body),
        path: Arc::from(path),
    })
}

fn parse_frontmatter(text: &str) -> Result<(Frontmatter, &str, usize), PromptProfileError> {
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Ok((Frontmatter::default(), text, 1));
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Ok((Frontmatter::default(), text, 1));
    }
    offset += first.len();
    let yaml_start = offset;
    let mut line_number = 1;
    for line in lines {
        line_number += 1;
        let line_start = offset;
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let yaml = &text[yaml_start..line_start];
            let frontmatter = if yaml.trim().is_empty() {
                Frontmatter::default()
            } else {
                serde_yaml::from_str(yaml)?
            };
            return Ok((frontmatter, &text[offset..], line_number + 1));
        }
    }
    Err(PromptProfileError::UnclosedFrontmatter)
}

fn validate_custom_template(
    template: &str,
    starting_line: usize,
) -> Result<(), PromptProfileError> {
    let mut directives = HashSet::new();
    for (index, line) in template.lines().enumerate() {
        let line_number = starting_line + index;
        if line.starts_with("\\{{maki.") && line.ends_with("}}") {
            continue;
        }
        let Some(name) = line
            .strip_prefix("{{maki.")
            .and_then(|line| line.strip_suffix("}}"))
        else {
            continue;
        };
        if !super::is_system_component(name) {
            return Err(PromptProfileError::UnknownDirective {
                directive: line.to_owned(),
                line: line_number,
            });
        }
        if !directives.insert(name) {
            return Err(PromptProfileError::DuplicateDirective {
                directive: line.to_owned(),
                line: line_number,
            });
        }
    }
    if directives.contains("default") && directives.len() > 1 {
        return Err(PromptProfileError::DefaultMixedWithComponents);
    }
    for required in ["tools", "context", "plan"] {
        if !directives.contains("default") && !directives.contains(required) {
            tracing::warn!(
                component = required,
                "custom system prompt profile omits Maki component"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn discover(dir: &Path) -> PromptProfileCatalog {
        PromptProfileCatalog::discover_with(None, Some(dir))
    }

    #[test]
    fn discovers_overlay_and_custom_profiles() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("review.md"), "Review carefully.").unwrap();
        fs::write(
            profiles.join("custom.md"),
            "---\ndescription: Custom layout\nlayout: custom\n---\n{{maki.default}}",
        )
        .unwrap();

        let catalog = discover(dir.path());
        let review = catalog.get("review").unwrap();
        assert_eq!(review.layout(), PromptProfileLayout::Overlay);
        assert_eq!(review.body(), "Review carefully.");
        let custom = catalog.get("custom").unwrap();
        assert_eq!(custom.layout(), PromptProfileLayout::Custom);
        assert_eq!(custom.description(), Some("Custom layout"));
    }

    #[test]
    fn invalid_profile_is_reported_when_selected() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("bad.md"), "---\nlayout: nope\n---\nbody").unwrap();

        let error = discover(dir.path()).resolve(Some("bad")).unwrap_err();
        assert!(error.to_string().contains("is invalid"));
    }

    #[test]
    fn oversized_profile_is_rejected_without_truncation() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("large.md"), vec![b'x'; MAX_PROFILE_BYTES + 1]).unwrap();

        let error = discover(dir.path()).resolve(Some("large")).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn preserves_indented_markdown_and_skips_non_files() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("code.md"), "    indented code\n").unwrap();
        fs::create_dir(profiles.join("directory.md")).unwrap();

        let catalog = discover(dir.path());
        assert_eq!(catalog.get("code").unwrap().body(), "    indented code");
        assert!(catalog.get("directory").is_none());
    }

    #[test]
    fn custom_template_rejects_unknown_duplicate_and_mixed_directives() {
        let unknown = validate_custom_template("{{maki.unknown}}", 1).unwrap_err();
        assert!(matches!(
            unknown,
            PromptProfileError::UnknownDirective { .. }
        ));
        let duplicate = validate_custom_template("{{maki.tools}}\n{{maki.tools}}", 1).unwrap_err();
        assert!(matches!(
            duplicate,
            PromptProfileError::DuplicateDirective { .. }
        ));
        let mixed = validate_custom_template("{{maki.default}}\n{{maki.plan}}", 1).unwrap_err();
        assert!(matches!(
            mixed,
            PromptProfileError::DefaultMixedWithComponents
        ));
    }

    #[test]
    fn custom_template_error_uses_file_line_number() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(
            profiles.join("bad.md"),
            "---\nlayout: custom\n---\n\n{{maki.unknown}}",
        )
        .unwrap();

        let error = discover(dir.path()).resolve(Some("bad")).unwrap_err();
        assert!(error.to_string().contains("line 5"));
    }

    #[test]
    fn builtin_and_missing_profiles_resolve_predictably() {
        let catalog = PromptProfileCatalog::default();
        assert!(catalog.resolve(None).unwrap().is_none());
        assert!(
            catalog
                .resolve(Some(BUILTIN_PROFILE_NAME))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            catalog.resolve(Some("missing")),
            Err(PromptProfileSelectionError::NotFound { .. })
        ));
    }

    #[test]
    fn overlay_keeps_dynamic_components_and_lands_before_plan() {
        const PROFILE_TEXT: &str = "PROFILE {{tone}} {{instructions}}";
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("review.md"), PROFILE_TEXT).unwrap();
        let profile = discover(dir.path()).get("review").unwrap();

        let output = crate::prompt::assemble_system(
            &crate::prompt::ResolvedSlots::default(),
            "RUNTIME_CONTEXT",
            "PLAN_REMINDER",
            Some(&profile),
        );
        assert!(output.contains("# Tool usage"));
        assert!(output.contains("RUNTIME_CONTEXT"));
        assert!(output.contains(PROFILE_TEXT));
        assert!(output.find(PROFILE_TEXT) < output.find("PLAN_REMINDER"));
    }

    #[test]
    fn custom_layout_reorders_components_and_can_omit_sections() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(
            profiles.join("custom.md"),
            "---\nlayout: custom\n---\nCUSTOM\n{{maki.context}}\n{{maki.identity}}\n{{maki.plan}}",
        )
        .unwrap();
        let profile = discover(dir.path()).get("custom").unwrap();

        let output = crate::prompt::assemble_system(
            &crate::prompt::ResolvedSlots::default(),
            "RUNTIME_CONTEXT",
            "PLAN_REMINDER",
            Some(&profile),
        );
        assert!(output.starts_with("CUSTOM"));
        assert!(output.find("RUNTIME_CONTEXT") < output.find("You are Maki"));
        assert!(!output.contains("# Tool usage"));
        assert!(output.ends_with("PLAN_REMINDER"));
    }

    #[test]
    fn custom_layout_can_escape_a_directive() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(
            profiles.join("escaped.md"),
            "---\nlayout: custom\n---\n\\{{maki.tools}}",
        )
        .unwrap();
        let profile = discover(dir.path()).get("escaped").unwrap();

        let output = crate::prompt::assemble_system(
            &crate::prompt::ResolvedSlots::default(),
            "",
            "",
            Some(&profile),
        );
        assert_eq!(output, "{{maki.tools}}");
    }
}
