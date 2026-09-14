use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MAX_PATH_BYTES: usize = 4096;
const MAX_COMPONENT_BYTES: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryNavigation(String);

impl DirectoryNavigation {
    pub fn new(path: impl Into<String>) -> Result<Self, WorkspacePathError> {
        let path = path.into();
        if path.len() > MAX_PATH_BYTES {
            return Err(WorkspacePathError::TooLong);
        }
        for component in path.split('/') {
            if !matches!(component, "." | "..") {
                WorkspacePath::new(component)?;
            }
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn resolve(&self, base: &WorkspacePath) -> Result<WorkspacePath, WorkspacePathError> {
        let mut target = base.clone();
        for component in self.0.split('/') {
            target = match component {
                "." => target,
                ".." => target.parent().ok_or(WorkspacePathError::Traversal)?,
                _ if target.is_root() => WorkspacePath::new(component)?,
                _ => WorkspacePath::new(format!("{target}/{component}"))?,
            };
        }
        Ok(target)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkspacePathError {
    #[error("workspace path must not be empty")]
    Empty,
    #[error("workspace path must be root-relative")]
    Absolute,
    #[error("workspace path contains an empty component")]
    EmptyComponent,
    #[error("workspace path contains a traversal component")]
    Traversal,
    #[error("workspace path must not contain a backslash")]
    Backslash,
    #[error("workspace path contains a control character")]
    ControlCharacter,
    #[error("workspace path exceeds {MAX_PATH_BYTES} bytes")]
    TooLong,
    #[error("workspace path component exceeds {MAX_COMPONENT_BYTES} bytes")]
    ComponentTooLong,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// A validated UTF-8 slash path relative to a workspace cursor.
pub struct WorkspacePath(WorkspacePathKind);

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum WorkspacePathKind {
    Root,
    Relative(String),
}

impl WorkspacePath {
    pub fn new(path: impl Into<String>) -> Result<Self, WorkspacePathError> {
        let path = path.into();
        if path.is_empty() {
            return Err(WorkspacePathError::Empty);
        }
        if path == "." {
            return Ok(Self::root());
        }
        if path.starts_with('/') {
            return Err(WorkspacePathError::Absolute);
        }
        if path.contains('\\') {
            return Err(WorkspacePathError::Backslash);
        }
        if path.len() > MAX_PATH_BYTES {
            return Err(WorkspacePathError::TooLong);
        }
        for component in path.split('/') {
            if component.is_empty() {
                return Err(WorkspacePathError::EmptyComponent);
            }
            if matches!(component, "." | "..") {
                return Err(WorkspacePathError::Traversal);
            }
            if component.len() > MAX_COMPONENT_BYTES {
                return Err(WorkspacePathError::ComponentTooLong);
            }
            if component.chars().any(char::is_control) {
                return Err(WorkspacePathError::ControlCharacter);
            }
        }
        Ok(Self(WorkspacePathKind::Relative(path)))
    }

    pub const fn root() -> Self {
        Self(WorkspacePathKind::Root)
    }

    pub const fn is_root(&self) -> bool {
        matches!(self.0, WorkspacePathKind::Root)
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            WorkspacePathKind::Root => ".",
            WorkspacePathKind::Relative(path) => path,
        }
    }

    pub fn file_name(&self) -> &str {
        self.as_str().rsplit('/').next().unwrap_or(self.as_str())
    }

    pub fn parent(&self) -> Option<Self> {
        match &self.0 {
            WorkspacePathKind::Root => None,
            WorkspacePathKind::Relative(path) => Some(
                path.rsplit_once('/')
                    .map_or_else(Self::root, |(parent, _)| {
                        Self(WorkspacePathKind::Relative(parent.to_owned()))
                    }),
            ),
        }
    }
}

impl fmt::Debug for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkspacePath")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WorkspacePath {
    type Err = WorkspacePathError;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        Self::new(path)
    }
}

impl Serialize for WorkspacePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WorkspacePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = String::deserialize(deserializer)?;
        Self::new(path).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{DirectoryNavigation, WorkspacePath, WorkspacePathError};

    const VALID_PATH: &str = "src/ordinary name/日本語.rs";

    #[test_case("a/b", "..", Some("a"); "parent")]
    #[test_case("a/b", "../sibling", Some("a/sibling"); "sibling")]
    #[test_case("a", "..", Some("."); "root")]
    #[test_case(".", "..", None; "escape")]
    #[test_case("a", "../../a", None; "escape_then_return")]
    fn navigation_is_root_confined(base: &str, navigation: &str, expected: Option<&str>) {
        let result = DirectoryNavigation::new(navigation)
            .unwrap()
            .resolve(&WorkspacePath::new(base).unwrap());
        assert_eq!(result.ok().as_ref().map(WorkspacePath::as_str), expected);
    }

    #[test_case("/tmp"; "absolute")]
    #[test_case("a//b"; "empty_component")]
    #[test_case("a\\b"; "backslash")]
    #[test_case("a\n"; "control")]
    fn navigation_rejects_invalid_syntax(path: &str) {
        assert!(DirectoryNavigation::new(path).is_err());
    }

    #[test_case("", WorkspacePathError::Empty; "empty")]
    #[test_case("/etc/passwd", WorkspacePathError::Absolute; "absolute")]
    #[test_case("src//lib.rs", WorkspacePathError::EmptyComponent; "empty_component")]
    #[test_case("src/", WorkspacePathError::EmptyComponent; "trailing_slash")]
    #[test_case("./src", WorkspacePathError::Traversal; "current_directory")]
    #[test_case("src/../secret", WorkspacePathError::Traversal; "parent_traversal")]
    #[test_case(r"src\lib.rs", WorkspacePathError::Backslash; "backslash")]
    #[test_case("src/\0secret", WorkspacePathError::ControlCharacter; "nul")]
    #[test_case("src/line\nbreak", WorkspacePathError::ControlCharacter; "control")]
    fn rejects_invalid_protocol_paths(input: &str, expected: WorkspacePathError) {
        assert_eq!(WorkspacePath::new(input), Err(expected));
    }

    #[test]
    fn accepts_and_round_trips_ordinary_utf8_names() {
        let path = WorkspacePath::new(VALID_PATH).expect("valid protocol path");
        let json = serde_json::to_string(&path).expect("serialize path");
        let restored: WorkspacePath = serde_json::from_str(&json).expect("deserialize path");

        assert_eq!(restored, path);
        assert_eq!(path.file_name(), "日本語.rs");
    }

    #[test]
    fn root_has_an_explicit_round_trippable_representation() {
        let root = WorkspacePath::new(".").expect("valid root path");

        assert!(root.is_root());
        assert_eq!(root, WorkspacePath::root());
        assert_eq!(root.as_str(), ".");
        assert_eq!(root.parent(), None);
        assert_eq!(
            serde_json::to_string(&root).expect("serialize root"),
            r#"".""#
        );
        assert_eq!(
            serde_json::from_str::<WorkspacePath>(r#"".""#).expect("deserialize root"),
            root
        );
    }

    #[test]
    fn a_top_level_path_has_root_as_its_parent() {
        let path = WorkspacePath::new("src").expect("valid path");

        assert_eq!(path.parent(), Some(WorkspacePath::root()));
    }

    #[test]
    fn rejects_overlong_components_and_paths() {
        let component = "x".repeat(256);
        assert_eq!(
            WorkspacePath::new(component),
            Err(WorkspacePathError::ComponentTooLong)
        );

        let path = std::iter::repeat_n("x".repeat(255), 17)
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(WorkspacePath::new(path), Err(WorkspacePathError::TooLong));
    }

    #[test]
    fn deserialization_cannot_bypass_validation() {
        let error = serde_json::from_str::<WorkspacePath>(r#""src/../secret""#)
            .expect_err("invalid serialized path");
        assert!(error.to_string().contains("traversal"));
    }
}
