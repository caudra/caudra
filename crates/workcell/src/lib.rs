#![forbid(unsafe_code)]

//! Protocol-neutral typed APIs for embedding Workcell tools.

pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};

#[cfg(feature = "environment")]
pub use workcell_environment as environment;
#[cfg(feature = "environment")]
pub use workcell_environment::ExecutionEnvironment;
#[cfg(feature = "code")]
pub use workcell_mcp_code as code;
#[cfg(feature = "code")]
pub use workcell_mcp_code::CodeToolGroup;
#[cfg(feature = "files")]
pub use workcell_mcp_files as files;
#[cfg(feature = "files")]
pub use workcell_mcp_files::{FileToolGroup, PreparedFilePatch};
#[cfg(feature = "shell")]
pub use workcell_mcp_shell as shell;
#[cfg(feature = "shell")]
pub use workcell_mcp_shell::{PreparedShell, ShellToolGroup};
#[cfg(feature = "web")]
pub use workcell_mcp_web as web;
#[cfg(feature = "web")]
pub use workcell_mcp_web::{PreparedWebfetch, PreparedWebsearch, WebToolGroup};
