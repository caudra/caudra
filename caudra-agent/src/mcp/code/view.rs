//! The servers and tools code mode presents, decoupled from the live MCP index so naming,
//! signatures, the catalog and search are pure functions of plain data.

use serde_json::Value;

/// Bytes of one server's instructions the catalog carries.
pub const MAX_SERVER_INSTRUCTIONS_BYTES: usize = 4096;
const INSTRUCTIONS_CUT_MARKER: &str = "\n[instructions cut]";

/// Where a server stands, as the catalog reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerState {
    Connected,
    Connecting,
    /// Waiting for the user to trust a project server.
    AwaitingTrust,
    NeedsLogin,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerView {
    /// The configured server name. Permission keys are `<name>.<tool>`.
    pub name: String,
    /// The server's own title from `serverInfo`, when it differs from `name`.
    pub title: Option<String>,
    pub state: ServerState,
    /// The server's `instructions`, already cut to their byte limit.
    pub instructions: Option<String>,
    /// Callable tools in any order. Disabled tools are never included.
    pub tools: Vec<ToolView>,
    /// Whether the server lists resources.
    pub resources: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolView {
    /// The tool's raw MCP name, as sent in `tools/call`.
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

/// A server's instructions as the catalog carries them: trimmed, and cut on a character
/// boundary past [`MAX_SERVER_INSTRUCTIONS_BYTES`] with a marker that says so.
pub fn cut_instructions(text: &str) -> String {
    let text = text.trim();
    let kept = &text[..text.floor_char_boundary(MAX_SERVER_INSTRUCTIONS_BYTES)];
    if kept.len() < text.len() {
        format!("{kept}{INSTRUCTIONS_CUT_MARKER}")
    } else {
        kept.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{INSTRUCTIONS_CUT_MARKER, MAX_SERVER_INSTRUCTIONS_BYTES, cut_instructions};

    const WIDE: &str = "é";

    #[test_case(MAX_SERVER_INSTRUCTIONS_BYTES - WIDE.len(), None ; "a_wide_char_that_fits_is_kept")]
    #[test_case(MAX_SERVER_INSTRUCTIONS_BYTES - 1, Some(MAX_SERVER_INSTRUCTIONS_BYTES - 1) ; "a_wide_char_across_the_cap_is_dropped_whole")]
    fn instructions_are_cut_on_a_char_boundary(prefix: usize, kept: Option<usize>) {
        let text = format!("{}{WIDE}", "a".repeat(prefix));
        let expected = match kept {
            Some(bytes) => format!("{}{INSTRUCTIONS_CUT_MARKER}", &text[..bytes]),
            None => text.clone(),
        };
        assert_eq!(cut_instructions(&text), expected);
    }
}
