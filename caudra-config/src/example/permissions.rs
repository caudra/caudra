use caudra_storage::permission_state::{COMMAND_PATTERN_MAX_BYTES, COMMAND_PATTERN_MAX_TOKENS};

use crate::files;
use crate::{McpPermissions, PERMISSIONS_VERSION, PermissionsFileConfig, ToolPermissions};

use super::{Document, Header, RECORDS_USAGE, Table, global_location, preamble};

pub const TOOL: &str = "shell";
pub const MCP_SERVER: &str = "mcp.github";
const PROJECT_SCOPE: &str = "A project .caudra/permissions.toml adds rules of its own. Deny and \
     ask rules from both files apply, and a project shell allow waits until you trust the project \
     policy in /permissions.";
const FAILS_CLOSED: &str = "A file that is unreadable, malformed, or newer than this build fails \
     closed: Caudra denies tool calls until you fix it.";
const MCP_ABOUT: &str = "Each [mcp.SERVER] table holds the rules for the tools of one MCP server, \
     such as [mcp.github] here. Name each tool as the server does, without the `github__` prefix.";

/// Every `permissions.toml` key, with example `[TOOL]` and `[mcp.SERVER]`
/// tables.
pub fn document() -> Document {
    let file = &files::PERMISSIONS;
    Document {
        preamble: preamble(
            file,
            [
                RECORDS_USAGE.to_owned(),
                format!("{} {PROJECT_SCOPE}", global_location(file)),
                FAILS_CLOSED.to_owned(),
            ],
        ),
        version: PERMISSIONS_VERSION,
        tables: vec![
            Table::of(Header::Root, PermissionsFileConfig::FIELDS),
            Table::of(Header::Record(TOOL.into()), ToolPermissions::FIELDS).about(tool_about()),
            Table::of(Header::Record(MCP_SERVER.into()), McpPermissions::FIELDS).about(MCP_ABOUT),
        ],
    }
}

fn tool_about() -> String {
    format!(
        "Each [TOOL] table holds the rules of one tool, such as [shell] here, and [\"*\"] holds \
         rules for every tool. A shell allow or ask pattern is literal words with an optional \
         final ` *`, such as `git status *`, in at most {COMMAND_PATTERN_MAX_TOKENS} words and \
         {COMMAND_PATTERN_MAX_BYTES} bytes. One invalid pattern makes the whole file fail closed."
    )
}

#[cfg(test)]
mod tests {
    use super::{MCP_SERVER, TOOL, document};
    use crate::example::Render;
    use crate::{McpPermissions, PermissionsFileConfig, build_permissions};

    /// A value each rule key takes, in both kinds of table, since none has a
    /// default to render.
    const RULE_SAMPLES: [(&str, &str); 4] = [
        ("allow", "true"),
        ("ask", "false"),
        ("deny", "[\"delete\"]"),
        ("default", "\"deny\""),
    ];

    fn policy(render: Render) -> String {
        let file: PermissionsFileConfig = toml::from_str(&document().render(render)).unwrap();
        format!(
            "{:?}",
            build_permissions(file, PermissionsFileConfig::default())
        )
    }

    #[test]
    fn the_reference_is_an_empty_policy() {
        let file: PermissionsFileConfig =
            toml::from_str(&document().render(Render::Reference)).unwrap();
        assert!(file.default.is_none() && file.tools.is_empty());
        assert_eq!(file.mcp, McpPermissions::default());
    }

    #[test]
    fn the_stated_default_is_the_built_in_one() {
        assert_eq!(
            policy(Render::Live { defaults: true }),
            policy(Render::Live { defaults: false })
        );
    }

    #[test]
    fn every_rule_key_is_one_the_parser_takes() {
        let document = document();
        let mut text = String::new();
        for path in [TOOL, MCP_SERVER] {
            let table = document.table(path).unwrap();
            let names: Vec<&str> = table.entries.iter().map(|entry| entry.name).collect();
            assert_eq!(names, RULE_SAMPLES.map(|(name, _)| name), "{path}");
            text.push_str(&format!("[{path}]\n"));
            for (name, value) in RULE_SAMPLES {
                text.push_str(&format!("{name} = {value}\n"));
            }
        }
        toml::from_str::<PermissionsFileConfig>(&text).unwrap();
    }
}
