use caudra_lua::docs_render;

use crate::page_header;

const LUA_NOTICE: &str = "Lua plugins are experimental and off by default. Turn them on with `lua_plugins = true` under `[experimental]` in the global `caudra.toml`, then restart Caudra. `--no-plugins` turns Lua off again for one run. See [Experimental features](/docs/configuration/#experimental-features).";

/// Removes the renderer's H1 and places the opt-in notice before its body.
pub fn with_lua_notice(page: &str) -> String {
    match page.split_once('\n') {
        Some((_, rest)) => format!("{LUA_NOTICE}\n{rest}"),
        None => format!("{LUA_NOTICE}\n"),
    }
}

pub fn generate() -> String {
    format!(
        "{}{}",
        page_header(
            "Writing caudra plugins",
            "Add your own tools and commands in Lua, or let the agent write them (experimental)."
        ),
        with_lua_notice(&docs_render::guide_page())
    )
}
