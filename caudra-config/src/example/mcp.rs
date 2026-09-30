use crate::MAX_SERVER_NAME_LEN;
use crate::files;
use crate::mcp::{
    HTTP_FIELDS, MCP_VERSION, OAUTH_FIELDS, SERVER_FIELDS, STDIO_FIELDS, TOP_LEVEL_FIELDS,
};

use super::{Document, Header, RECORDS_USAGE, Table, global_location, preamble};

pub const STDIO_SERVER: &str = "mcp.filesystem";
pub const HTTP_SERVER: &str = "mcp.analytics";
pub const OAUTH_CLIENT: &str = "mcp.analytics.oauth";
const PROJECT_SCOPE: &str = "A project .caudra/mcp.toml adds servers, and a project server \
     replaces a global server with the same name. A project server that runs a command, or that \
     reaches a private address, waits until you review it in /mcp.";
const SAVES: &str = "When /mcp turns a server on or off, Caudra sets `enabled` in the file that \
     defines the server and keeps your comments.";
const HTTP_ABOUT: &str = "An HTTP server connects to `url` instead of running a command. \
     `enabled`, `timeout`, and `always_load` work here too.";
const OAUTH_ABOUT: &str = "The static OAuth client of [mcp.analytics], for a server that has no \
     dynamic client registration. Without it, Caudra registers a client when the server asks for \
     a login.";

/// Every `mcp.toml` key, on an example stdio server and an example HTTP
/// server.
pub fn document() -> Document {
    let file = &files::MCP;
    Document {
        preamble: preamble(
            file,
            [
                RECORDS_USAGE.to_owned(),
                format!("{} {PROJECT_SCOPE}", global_location(file)),
                SAVES.to_owned(),
            ],
        ),
        version: MCP_VERSION,
        tables: vec![
            Table::of(Header::Root, TOP_LEVEL_FIELDS),
            Table::of(
                Header::Record(STDIO_SERVER.into()),
                SERVER_FIELDS.iter().chain(STDIO_FIELDS),
            )
            .about(server_about()),
            Table::of(Header::Record(HTTP_SERVER.into()), HTTP_FIELDS).about(HTTP_ABOUT),
            Table::of(Header::Record(OAUTH_CLIENT.into()), OAUTH_FIELDS).about(OAUTH_ABOUT),
        ],
    }
}

fn server_about() -> String {
    format!(
        "Each [mcp.NAME] table is one server, and a stdio server like this one runs `command`. \
         NAME is 1 to {MAX_SERVER_NAME_LEN} ASCII letters, digits, and hyphens, and cannot be \
         the name of a built-in tool. The tools of the server are named NAME__TOOL."
    )
}
