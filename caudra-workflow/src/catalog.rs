//! What a workflow discovery reports: every script that could be launched,
//! every file that could not, and the shape of a launch.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::snapshot::SourceKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
    pub source_kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub digest: String,
    pub trusted: bool,
    /// Scopes holding a same-named script this entry takes precedence over.
    #[serde(default)]
    pub shadowed: Vec<SourceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidEntry {
    pub path: String,
    pub source_kind: SourceKind,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCatalog {
    #[serde(default)]
    pub entries: Vec<CatalogEntry>,
    #[serde(default)]
    pub invalid: Vec<InvalidEntry>,
    /// Where a project script goes. Absent when the session has no project
    /// root, so a writer knows not to invent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<PathBuf>,
    /// Where a user script goes, resolved for this build and platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub name: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_budget: Option<u32>,
}
