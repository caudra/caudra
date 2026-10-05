use caudra_lua::docs_render;

use crate::gen_plugins::with_lua_notice;
use crate::page_header;

pub fn generate() -> String {
    format!(
        "{}{}",
        page_header(
            "Lua API",
            "The plugin surface, mirrored from Neovim (experimental)."
        ),
        with_lua_notice(&docs_render::site_page())
    )
}
