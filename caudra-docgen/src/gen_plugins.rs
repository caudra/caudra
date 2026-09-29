use caudra_lua::docs_render;

const FRONTMATTER: &str = r#"+++
title = "Plugins"
weight = 23
[extra]
group = "Guides"
+++

"#;

const LUA_NOTICE: &str = "Lua plugins are experimental and off by default. Turn them on with `lua_plugins = true` under `[experimental]` in the global `caudra.toml`, then restart Caudra. `--no-plugins` turns Lua off again for one run. See [Experimental features](/docs/configuration/#experimental-features).";

/// Places the opt-in notice under the page title, ahead of anything that
/// assumes Lua runs.
pub fn with_lua_notice(page: &str) -> String {
    match page.split_once('\n') {
        Some((title, rest)) => format!("{title}\n\n{LUA_NOTICE}\n{rest}"),
        None => format!("{page}\n\n{LUA_NOTICE}\n"),
    }
}

pub fn generate() -> String {
    format!(
        "{FRONTMATTER}{}",
        with_lua_notice(&docs_render::guide_page())
    )
}
