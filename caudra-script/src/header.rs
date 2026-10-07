use rhai::{ASTFlags, Expr, OptimizationLevel, ParseError, Position, Stmt};
use serde_json::{Map, Number, Value};

use crate::sandbox::{SandboxLimits, bounded_engine};

/// A scalar literal a header may hold. Arrays and maps of accepted literals are always accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    String,
    Integer,
    Float,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("script failed to parse: {0}")]
    Parse(ParseError),
    #[error("the first statement does not bind the header")]
    NotFirst,
    #[error("the header holds something other than an accepted literal ({position})")]
    NonLiteral { position: Position },
    #[error("the header holds a float that is not finite ({position})")]
    NonFinite { position: Position },
}

/// Reads the header without running the script: the first statement must be
/// `let <variable> = <literal>`, built from `scalars`, arrays and maps. The parse applies the
/// sandbox's limits and disabled symbols, and no optimisation, so nothing folds into a literal.
pub fn parse_header(
    source: &str,
    limits: &SandboxLimits,
    variable: &str,
    scalars: &[ScalarKind],
) -> Result<Value, HeaderError> {
    let mut engine = bounded_engine(limits);
    engine.set_optimization_level(OptimizationLevel::None);
    let ast = engine.compile(source).map_err(HeaderError::Parse)?;
    match ast.statements().first() {
        Some(Stmt::Var(binding, flags, _))
            if !flags.contains(ASTFlags::CONSTANT) && binding.0.name.as_str() == variable =>
        {
            literal_to_json(&binding.1, scalars)
        }
        _ => Err(HeaderError::NotFirst),
    }
}

fn literal_to_json(expr: &Expr, scalars: &[ScalarKind]) -> Result<Value, HeaderError> {
    let accepts = |kind| scalars.contains(&kind);
    match expr {
        Expr::StringConstant(text, _) if accepts(ScalarKind::String) => {
            Ok(Value::String(text.to_string()))
        }
        Expr::IntegerConstant(number, _) if accepts(ScalarKind::Integer) => {
            Ok(Value::from(*number))
        }
        Expr::FloatConstant(number, position) if accepts(ScalarKind::Float) => {
            Number::from_f64(**number)
                .map(Value::Number)
                .ok_or(HeaderError::NonFinite {
                    position: *position,
                })
        }
        Expr::BoolConstant(value, _) if accepts(ScalarKind::Bool) => Ok(Value::Bool(*value)),
        Expr::Array(items, _) => items
            .iter()
            .map(|item| literal_to_json(item, scalars))
            .collect::<Result<Vec<Value>, HeaderError>>()
            .map(Value::Array),
        Expr::Map(map, _) => map
            .0
            .iter()
            .map(|(key, value)| Ok((key.name.to_string(), literal_to_json(value, scalars)?)))
            .collect::<Result<Map<String, Value>, HeaderError>>()
            .map(Value::Object),
        other => Err(HeaderError::NonLiteral {
            position: other.position(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const VARIABLE: &str = "meta";
    const STRINGS: [ScalarKind; 1] = [ScalarKind::String];
    const SCALARS: [ScalarKind; 4] = [
        ScalarKind::String,
        ScalarKind::Integer,
        ScalarKind::Float,
        ScalarKind::Bool,
    ];
    const LIMITS: SandboxLimits = SandboxLimits {
        max_operations: 1_000,
        max_call_levels: 8,
        max_expr_depth: 16,
        max_string_size: 1_024,
        max_array_size: 64,
        max_map_size: 64,
    };
    const DEEPER_THAN_LIMIT: &str = "let meta = [[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]];";
    const VARIABLE_VALUE: &str = "let meta = #{ a: x };";
    const VARIABLE_LINE: u16 = 1;
    const VARIABLE_COLUMN: u16 = 18;
    const INFINITE: &str = "1e999";
    const VALID_HEADER: &str = "the header is valid";

    /// The body after the header calls a function nothing defines, so a header that parses
    /// proves the script never ran.
    fn header(literal: &str, scalars: &[ScalarKind]) -> Result<Value, HeaderError> {
        parse_header(
            &format!("let {VARIABLE} = {literal};\nundefined_host_call();"),
            &LIMITS,
            VARIABLE,
            scalars,
        )
    }

    #[test_case(r#""text""# => json!("text"); "string")]
    #[test_case("42" => json!(42); "integer")]
    #[test_case("-42" => json!(-42); "negative_integer")]
    #[test_case("1.5" => json!(1.5); "float")]
    #[test_case("-0.25" => json!(-0.25); "negative_float")]
    #[test_case("true" => json!(true); "bool")]
    #[test_case(r#"[1, "a", [false]]"# => json!([1, "a", [false]]); "array")]
    #[test_case("#{ b: -1, a: #{ c: 2.5 } }" => json!({ "b": -1, "a": { "c": 2.5 } }); "nested_map")]
    fn every_accepted_literal_kind_converts(literal: &str) -> Value {
        header(literal, &SCALARS).expect(VALID_HEADER)
    }

    #[test_case("42"; "integer")]
    #[test_case("-42"; "negative_integer")]
    #[test_case("1.5"; "float")]
    #[test_case("true"; "bool")]
    #[test_case(r#"#{ a: ["x", 1] }"#; "nested_integer")]
    fn scalar_kinds_outside_the_accepted_set_are_rejected(literal: &str) {
        assert!(matches!(
            header(literal, &STRINGS),
            Err(HeaderError::NonLiteral { .. })
        ));
    }

    #[test_case("()"; "unit")]
    #[test_case("'c'"; "char")]
    #[test_case("1 + 2"; "arithmetic")]
    #[test_case(r#""a" + "b""#; "concatenation")]
    #[test_case("`x${1}`"; "interpolation")]
    #[test_case("args.x"; "property")]
    #[test_case("make()"; "call")]
    fn expressions_are_not_literals(literal: &str) {
        assert!(matches!(
            header(literal, &SCALARS),
            Err(HeaderError::NonLiteral { .. })
        ));
    }

    #[test]
    fn a_non_literal_reports_where_it_is() {
        assert_eq!(
            parse_header(VARIABLE_VALUE, &LIMITS, VARIABLE, &SCALARS),
            Err(HeaderError::NonLiteral {
                position: Position::new(VARIABLE_LINE, VARIABLE_COLUMN),
            })
        );
    }

    #[test]
    fn an_infinite_float_is_rejected() {
        assert!(matches!(
            header(INFINITE, &SCALARS),
            Err(HeaderError::NonFinite { .. })
        ));
    }

    #[test_case("let other = 1;\nlet meta = 1;"; "later_statement")]
    #[test_case("const meta = 1;"; "constant")]
    #[test_case("let other = 1;"; "other_variable")]
    #[test_case("meta = 1;"; "assignment")]
    #[test_case("// only a comment"; "no_statement")]
    #[test_case(""; "empty")]
    fn the_header_must_be_the_first_let(source: &str) {
        assert_eq!(
            parse_header(source, &LIMITS, VARIABLE, &SCALARS),
            Err(HeaderError::NotFirst)
        );
    }

    #[test]
    fn comments_before_the_header_are_skipped() {
        assert_eq!(
            parse_header(
                "// one\n/* two */\nlet meta = 1;",
                &LIMITS,
                VARIABLE,
                &SCALARS
            ),
            Ok(json!(1))
        );
    }

    #[test_case("let meta = #{"; "unterminated_map")]
    #[test_case(r#"let meta = 1; eval("2");"#; "disabled_symbol")]
    #[test_case(DEEPER_THAN_LIMIT; "deeper_than_the_limit")]
    fn parse_errors_are_reported(source: &str) {
        assert!(matches!(
            parse_header(source, &LIMITS, VARIABLE, &SCALARS),
            Err(HeaderError::Parse(_))
        ));
    }
}
