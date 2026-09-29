use caudra_lua::docs_render;

use crate::gen_plugins::with_lua_notice;

const FRONTMATTER: &str = r#"+++
title = "Lua API"
weight = 10
[extra]
group = "Reference"
+++

"#;

pub fn generate() -> String {
    format!(
        "{FRONTMATTER}{}",
        with_lua_notice(&docs_render::site_page())
    )
}
