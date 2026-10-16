use caudra_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Value};

use super::util::pair::{Pair, pair, try_pair};

/// Turn a Lua value into a YAML string. Most Lua types work, but
/// circular references will return an error.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) YAML string, or nil plus an error.
/// @example
/// local s, err = caudra.yaml.encode({ name = "caudra", tags = { "ai", "agent" } })
/// print(s)
#[lua_fn]
fn encode(lua: &Lua, value: Value) -> LuaResult<Pair<String>> {
    let serde_val: serde_yaml::Value = try_pair!(lua.from_value(value));
    Ok(pair(serde_yaml::to_string(&serde_val)))
}

/// Parse a YAML string into a Lua value. Mappings become tables and
/// sequences become 1-indexed arrays.
///
/// @param str string YAML string to decode.
/// @return (any?, string?) Decoded value, or nil plus an error.
/// @example
/// local t, err = caudra.yaml.decode("name: caudra\nversion: 1")
/// print(t.name) -- caudra
#[lua_fn]
fn decode(lua: &Lua, str: String) -> LuaResult<Pair<Value>> {
    let value = try_pair!(serde_yaml::from_str::<serde_yaml::Value>(&str));
    Ok((Some(lua.to_value(&value)?), None))
}

lua_table! {
    /// YAML encoding and decoding. Works the same way as `caudra.json`,
    /// but for YAML formatted strings.
    ///
    /// ```lua
    /// local t = caudra.yaml.decode("greeting: hello")
    /// print(t.greeting)
    /// ```
    "caudra.yaml" => pub(crate) fn create_yaml_table(), DOCS [
        encode, decode,
    ]
}

#[cfg(test)]
mod tests {
    use mlua::Lua;
    use test_case::test_case;

    const RECURSION_LIMIT: &str = "recursion limit exceeded";
    const REPETITION_LIMIT: &str = "repetition limit exceeded";
    const MAX_YAML_DEPTH: usize = 128;

    fn lua_with_yaml() -> Lua {
        let lua = Lua::new();
        let yaml = super::create_yaml_table(&lua).unwrap();
        lua.globals().set("yaml", yaml).unwrap();
        lua
    }

    #[test]
    fn decode_string() {
        let lua = lua_with_yaml();
        let result: i64 = lua
            .load(r#"local t, err = yaml.decode('x: 42'); return t.x"#)
            .eval()
            .unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn decode_error_returns_nil_and_message() {
        let lua = lua_with_yaml();
        let (is_nil, has_err): (bool, bool) = lua
            .load(r#"local t, err = yaml.decode(":\n  - :\n  bad"); return t == nil, err ~= nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test_case("a: 1\na: 2"; "duplicate_key")]
    #[test_case("a: *missing"; "unknown_alias")]
    #[test_case("a: 1\n---\nb: 2"; "multiple_documents")]
    fn invalid_yaml_returns_a_pair(input: &str) {
        let lua = lua_with_yaml();
        lua.globals().set("input", input).unwrap();
        let result: (bool, bool) = lua
            .load(
                "local value, err = yaml.decode(input); return value == nil, type(err) == 'string'",
            )
            .eval()
            .unwrap();
        assert_eq!(result, (true, true));
    }

    #[test_case("&a [*a]".to_owned(), RECURSION_LIMIT; "recursive_alias")]
    #[test_case(format!("{}0{}", "[".repeat(MAX_YAML_DEPTH + 1), "]".repeat(MAX_YAML_DEPTH + 1)), RECURSION_LIMIT; "deep_sequence")]
    #[test_case("a: &a [one, two]\nb: &b [*a,*a,*a,*a,*a,*a,*a,*a]\nc: &c [*b,*b,*b,*b,*b,*b,*b,*b]\nd: &d [*c,*c,*c,*c,*c,*c,*c,*c]\ne: [*d,*d,*d,*d,*d,*d,*d,*d]".to_owned(), REPETITION_LIMIT; "alias_expansion")]
    fn yaml_resource_limits_return_errors(input: String, expected: &str) {
        let lua = lua_with_yaml();
        lua.globals().set("input", input).unwrap();
        let (is_nil, error): (bool, String) = lua
            .load("local value, err = yaml.decode(input); return value == nil, err")
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(error.starts_with(expected), "{error}");
    }

    #[test_case("local t = yaml.decode('a: &a [true, yes, 42, 0x10]\\nb: *a'); return t.a[1] == true and t.b[2] == 'yes' and t.b[3] == 42 and t.b[4] == 16"; "aliases_and_scalar_types")]
    #[test_case("local t = yaml.decode('!thing {name: test}'); return t['!thing'].name == 'test'"; "tagged_mapping")]
    #[test_case("local t = yaml.decode('42: number\\ntrue: boolean'); local s = yaml.encode(t); local r = yaml.decode(s); return r[42] == 'number' and r[true] == 'boolean'"; "nonstring_keys")]
    #[test_case("local t = yaml.decode('[.nan, .inf, -.inf]'); local s = yaml.encode(t); local r = yaml.decode(s); return r[1] ~= r[1] and r[2] == math.huge and r[3] == -math.huge"; "nonfinite_roundtrip")]
    fn yaml_values_keep_lua_semantics(script: &str) {
        assert!(lua_with_yaml().load(script).eval::<bool>().unwrap());
    }

    #[test]
    fn roundtrip() {
        let lua = lua_with_yaml();
        let result: String = lua
            .load(
                r#"
                local t = {name = "test", count = 3}
                local s = yaml.encode(t)
                local t2 = yaml.decode(s)
                return t2.name .. ":" .. tostring(t2.count)
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "test:3");
    }

    #[test]
    fn encode_error_returns_nil_and_message() {
        let lua = lua_with_yaml();
        let (is_nil, has_err): (bool, bool) = lua
            .load(
                r#"
                local bad = {}
                bad.self_ref = bad
                local s, err = yaml.encode(bad)
                return s == nil, err ~= nil
                "#,
            )
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }
}
