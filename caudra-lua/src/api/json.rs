use caudra_lua_macro::{lua_fn, lua_table};
use mlua::{AnyUserData, Lua, LuaSerdeExt, Result as LuaResult, UserData, UserDataMethods, Value};

use super::util::convert::{json_to_lua, lua_to_json};
use super::util::pair::{Pair, pair, try_pair};

pub(crate) const VALIDATOR_DOCS: crate::docs::ModuleDoc = crate::docs::ModuleDoc {
    name: "caudra.json.SchemaValidator",
    kind: crate::docs::DocKind::Class,
    desc: "A compiled JSON Schema validator. Create one with \
`caudra.json.schema_validator()` and reuse it to validate many values \
without recompiling the schema each time.",
    fns: &[crate::docs::FnDoc {
        name: "validate",
        args: "{value}",
        desc: "Check {value} against the compiled schema. Returns nil \
when the value is valid. When validation fails, returns a list of \
human-readable error strings.",
        params: &[crate::docs::ParamDoc {
            name: "{value}",
            ty: "any",
            desc: "The Lua value to validate.",
        }],
        returns: "(table?) Array of error strings, or nil if valid.",
        guard: None,
        example: "local errs = validator:validate({ name = 123 })\n\
if errs then\n\
  for _, msg in ipairs(errs) do print(msg) end\n\
end",
    }],
};

/// Schema compile errors surface at creation time, before spending any tokens.
struct LuaSchemaValidator {
    validator: jsonschema::Validator,
}

impl UserData for LuaSchemaValidator {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("validate", |lua, this, value: Value| {
            let json = lua_to_json(lua, &value)?;
            let errors: Vec<String> = this
                .validator
                .iter_errors(&json)
                .map(|e| {
                    let path = e.instance_path().to_string();
                    if path.is_empty() {
                        e.to_string()
                    } else {
                        format!("at {path}: {e}")
                    }
                })
                .collect();
            if errors.is_empty() {
                return Ok(Value::Nil);
            }
            let tbl = lua.create_table()?;
            for (i, err) in errors.into_iter().enumerate() {
                tbl.set(i + 1, err)?;
            }
            Ok(Value::Table(tbl))
        });
    }
}

/// Turn a Lua value into a JSON string. Tables, strings, numbers,
/// booleans, and nil all work. Functions and userdata cannot be
/// serialized.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) JSON string, or nil plus an error.
/// @example
/// local s, err = caudra.json.encode({ name = "caudra", version = 1 })
/// print(s) -- {"name":"caudra","version":1}
#[lua_fn]
fn encode(lua: &Lua, value: Value) -> LuaResult<Pair<String>> {
    let serde_val: serde_json::Value = try_pair!(lua.from_value(value));
    Ok(pair(serde_json::to_string(&serde_val)))
}

/// Parse a JSON string into a Lua value. Objects become tables and
/// arrays become 1-indexed sequences.
///
/// @param str string JSON string to decode.
/// @return (any?, string?) Decoded value, or nil plus an error.
/// @example
/// local t, err = caudra.json.decode('{"x": 42}')
/// print(t.x) -- 42
#[lua_fn]
fn decode(lua: &Lua, str: String) -> LuaResult<Pair<Value>> {
    let value = try_pair!(serde_json::from_str::<serde_json::Value>(&str));
    Ok((Some(json_to_lua(lua, &value)?), None))
}

/// Compile a JSON Schema into a reusable validator object. Supports
/// draft-07, 2019-09, and 2020-12. Schema errors show up right away so
/// you catch mistakes before doing any real work.
///
/// @param schema table JSON Schema as a Lua table.
/// @return (caudra.json.SchemaValidator?, string?) Validator, or nil plus an error.
/// @example
/// local v, err = caudra.json.schema_validator({
///   type = "object",
///   properties = { name = { type = "string" } },
///   required = { "name" },
/// })
/// local errs = v:validate({ name = "caudra" })
/// assert(errs == nil)
#[lua_fn]
fn schema_validator(lua: &Lua, schema: Value) -> LuaResult<Pair<AnyUserData>> {
    let schema_json = try_pair!(lua_to_json(lua, &schema));
    let validator = try_pair!(jsonschema::options().offline().build(&schema_json));
    Ok((
        Some(lua.create_userdata(LuaSchemaValidator { validator })?),
        None,
    ))
}

lua_table! {
    /// JSON encoding, decoding, and schema validation. Encode Lua
    /// tables to JSON strings, decode JSON back into tables, and
    /// optionally validate data against a JSON Schema.
    ///
    /// ```lua
    /// local s = caudra.json.encode({ ok = true })
    /// local t = caudra.json.decode(s)
    /// ```
    "caudra.json" => pub(crate) fn create_json_table(), DOCS [
        encode, decode, schema_validator,
    ]
}

#[cfg(test)]
mod tests {
    use mlua::Lua;
    use serde_json::{Value as JsonValue, json};
    use test_case::test_case;

    use super::json_to_lua;

    const RETRIEVAL_DISABLED: &str = "Retrieval is disabled";
    const ANSWER_PATH: &str = "at /answer: ";

    fn lua_with_json() -> Lua {
        let lua = Lua::new();
        let json = super::create_json_table(&lua).unwrap();
        lua.globals().set("json", json).unwrap();
        lua
    }

    #[test_case("http://json-schema.org/draft-07/schema#"; "draft_07")]
    #[test_case("https://json-schema.org/draft/2019-09/schema"; "draft_2019_09")]
    #[test_case("https://json-schema.org/draft/2020-12/schema"; "draft_2020_12")]
    fn schema_validator_resolves_local_references_and_preserves_return_shapes(draft: &str) {
        let lua = lua_with_json();
        let schema = json!({
            "$schema": draft,
            "type": "object",
            "definitions": { "answer": { "type": "string" } },
            "properties": { "answer": { "$ref": "#/definitions/answer" } },
            "required": ["answer"],
        });
        lua.globals()
            .set("schema", json_to_lua(&lua, &schema).unwrap())
            .unwrap();
        let (no_compile_error, errors, valid): (bool, Vec<String>, bool) = lua
            .load(
                r#"
                local validator, err = json.schema_validator(schema)
                local errors = validator:validate({ answer = 42 })
                return err == nil, errors, validator:validate({ answer = "ok" }) == nil
                "#,
            )
            .eval()
            .unwrap();
        assert!(no_compile_error);
        assert!(valid);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with(ANSWER_PATH), "{errors:?}");
    }

    #[test_case(json!({ "type": "object", "properties": 7 }), false; "malformed_schema")]
    #[test_case(json!({ "$ref": "#/definitions/missing" }), false; "missing_local_reference")]
    #[test_case(json!({ "$ref": "http://127.0.0.1:9/schema.json" }), true; "http_reference")]
    #[test_case(json!({ "$ref": "https://127.0.0.1:9/schema.json" }), true; "https_reference")]
    #[test_case(json!({ "$ref": "file:///schema.json" }), true; "file_reference")]
    fn schema_compile_errors_return_a_pair_without_fetching(schema: JsonValue, offline: bool) {
        let lua = lua_with_json();
        lua.globals()
            .set("schema", json_to_lua(&lua, &schema).unwrap())
            .unwrap();
        let (is_nil, error): (bool, String) = lua
            .load("local validator, err = json.schema_validator(schema); return validator == nil, err")
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(!error.is_empty());
        if offline {
            assert!(error.contains(RETRIEVAL_DISABLED), "{error}");
        }
    }

    #[test]
    fn encode_table() {
        let lua = lua_with_json();
        let result: String = lua
            .load(r#"local s, err = json.encode({a = 1}); return s"#)
            .eval()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn decode_string() {
        let lua = lua_with_json();
        let result: i64 = lua
            .load(r#"local t, err = json.decode('{"x":42}'); return t.x"#)
            .eval()
            .unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn encode_error_returns_nil_and_message() {
        let lua = lua_with_json();
        let (is_nil, has_err): (bool, bool) = lua
            .load(r#"local s, err = json.encode(json.encode); return s == nil, err ~= nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test]
    fn decode_error_returns_nil_and_message() {
        let lua = lua_with_json();
        let (is_nil, has_err): (bool, bool) = lua
            .load(r#"local t, err = json.decode("{invalid}"); return t == nil, err ~= nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test]
    fn roundtrip() {
        let lua = lua_with_json();
        let result: String = lua
            .load(
                r#"
                local t = {name = "test", count = 3}
                local s = json.encode(t)
                local t2 = json.decode(s)
                return t2.name .. ":" .. tostring(t2.count)
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "test:3");
    }

    #[test]
    fn decode_array() {
        let lua = lua_with_json();
        let result: i64 = lua
            .load(r#"local t = json.decode('[10,20,30]'); return #t"#)
            .eval()
            .unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn encode_decode_empty_array_roundtrips() {
        let lua = lua_with_json();
        let result: String = lua
            .load(r#"local s = json.encode(json.decode('[]')); return s"#)
            .eval()
            .unwrap();
        assert_eq!(result, "[]");
    }

    #[test]
    fn decode_null_roundtrips() {
        let lua = lua_with_json();
        let result: String = lua
            .load(
                r#"
                local t = json.decode('{"a":null,"b":1}')
                local s = json.encode(t)
                local t2 = json.decode(s)
                return tostring(t2.b)
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "1");
    }
}
