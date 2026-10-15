use caudra_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult};

use super::util::pair::{Pair, pair};

/// Convert an HTML string to Markdown.
/// Useful for cleaning up web content fetched with `caudra.webfetch`.
///
/// @param html string HTML source text.
/// @return (string?, string?) Markdown text on success, or nil plus an error message.
/// @example
/// local md, err = caudra.text.html_to_markdown("<h1>Hello</h1><p>world</p>")
/// if err then return end
/// print(md) -- "# Hello\n\nworld"
#[lua_fn]
fn html_to_markdown(_lua: &Lua, html: String) -> LuaResult<Pair<String>> {
    Ok(pair(
        htmd::convert(&html).map_err(|e| format!("html_to_markdown: {e}")),
    ))
}

lua_table! {
    /// Text transformation utilities.
    ///
    /// Helper functions for converting between text formats.
    ///
    /// ```lua
    /// local md = caudra.text.html_to_markdown(html)
    /// ```
    "caudra.text" => pub(crate) fn create_text_table(), DOCS [
        html_to_markdown,
    ]
}

#[cfg(test)]
mod tests {
    use mlua::{Function, Lua};
    use test_case::test_case;

    use super::create_text_table;
    use crate::api::util::pair::Pair;

    const SPANNED_CODE: &str = "<pre><code class=language-rust><span class=line><span>let first = 1;\n</span></span><span class=line>\n</span><span class=line>  let second = 2;\n</span></code></pre>";
    const CODE_MARKDOWN: &str = "```rust\nlet first = 1;\n\n  let second = 2;\n```";

    #[test_case(SPANNED_CODE, CODE_MARKDOWN; "highlighted_code_newlines")]
    #[test_case("<p>Fish &amp; chips: <a href='/source'>details</a>.</p>", "Fish & chips: [details](/source)."; "entities_and_links")]
    #[test_case("<pre><code>`````\nexample\n`````</code></pre>", "``````\n`````\nexample\n`````\n``````"; "noncolliding_code_fence")]
    fn html_conversion_returns_markdown_and_no_error(html: &str, expected: &str) {
        let lua = Lua::new();
        let table = create_text_table(&lua).unwrap();
        let convert: Function = table.get("html_to_markdown").unwrap();
        let (markdown, error): Pair<String> = convert.call(html).unwrap();
        assert_eq!(markdown.as_deref(), Some(expected));
        assert_eq!(error, None);
    }
}
