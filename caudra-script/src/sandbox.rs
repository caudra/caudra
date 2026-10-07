use rhai::{Dynamic, Engine, EvalAltResult};

const DISABLED_SYMBOLS: [&str; 3] = ["eval", "print", "debug"];
const SLEEP: &str = "sleep";
const EXIT: &str = "exit";

/// Every bound a sandboxed engine enforces while it parses and runs a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLimits {
    pub max_operations: u64,
    pub max_call_levels: usize,
    pub max_expr_depth: usize,
    pub max_string_size: usize,
    pub max_array_size: usize,
    pub max_map_size: usize,
}

/// A full engine with `eval`, `print` and `debug` disabled, every limit applied, and `sleep` and
/// `exit` stubbed to fail with "`name`() is unavailable in `hint`", where the hint names the
/// scripts and how to end one. `import` and `timestamp` are compiled out by the `no_module` and
/// `no_time` features.
pub fn restricted_engine(limits: &SandboxLimits, hint: &'static str) -> Engine {
    let mut engine = bounded_engine(limits);
    engine.register_fn(SLEEP, move |_seconds: i64| unavailable(SLEEP, hint));
    engine.register_fn(SLEEP, move |_seconds: f64| unavailable(SLEEP, hint));
    engine.register_fn(EXIT, move || unavailable(EXIT, hint));
    engine.register_fn(EXIT, move |_value: Dynamic| unavailable(EXIT, hint));
    engine
}

/// The limits and disabled symbols without the stubs: everything that shapes a parse.
pub(crate) fn bounded_engine(limits: &SandboxLimits) -> Engine {
    let mut engine = Engine::new();
    engine
        .set_max_operations(limits.max_operations)
        .set_max_call_levels(limits.max_call_levels)
        .set_max_expr_depths(limits.max_expr_depth, limits.max_expr_depth)
        .set_max_string_size(limits.max_string_size)
        .set_max_array_size(limits.max_array_size)
        .set_max_map_size(limits.max_map_size);
    for symbol in DISABLED_SYMBOLS {
        engine.disable_symbol(symbol);
    }
    engine
}

fn unavailable(name: &str, hint: &str) -> Result<(), Box<EvalAltResult>> {
    Err(format!("{name}() is unavailable in {hint}").into())
}

#[cfg(test)]
mod tests {
    use rhai::ParseErrorType;
    use test_case::test_case;

    use super::*;

    const HINT: &str = "test scripts; end one with return";
    const LIMITS: SandboxLimits = SandboxLimits {
        max_operations: 100_000,
        max_call_levels: 8,
        max_expr_depth: 16,
        max_string_size: 64,
        max_array_size: 8,
        max_map_size: 8,
    };
    const DEEPER_THAN_LIMIT: &str = "let x = [[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]];";
    const MUST_FAIL: &str = "the sandbox must stop this script";

    fn innermost(error: EvalAltResult) -> EvalAltResult {
        match error {
            EvalAltResult::ErrorInFunctionCall(_, _, inner, _) => innermost(*inner),
            other => other,
        }
    }

    fn failure(engine: &Engine, source: &str) -> EvalAltResult {
        innermost(*engine.run(source).expect_err(MUST_FAIL))
    }

    #[test_case(r#"eval("1");"#; "eval")]
    #[test_case("print(1);"; "print")]
    #[test_case("debug(1);"; "debug")]
    fn disabled_symbols_fail_to_compile(source: &str) {
        assert!(restricted_engine(&LIMITS, HINT).compile(source).is_err());
    }

    #[test_case("sleep(1);", SLEEP; "sleep_int")]
    #[test_case("sleep(0.5);", SLEEP; "sleep_float")]
    #[test_case("exit();", EXIT; "exit")]
    #[test_case("exit(1);", EXIT; "exit_value")]
    fn stubs_fail_with_the_hint(source: &str, name: &str) {
        match failure(&restricted_engine(&LIMITS, HINT), source) {
            EvalAltResult::ErrorRuntime(message, _) => assert_eq!(
                message.into_string(),
                Ok(format!("{name}() is unavailable in {HINT}"))
            ),
            other => panic!("{MUST_FAIL}: {other:?}"),
        }
    }

    #[test_case("loop {}" => matches EvalAltResult::ErrorTooManyOperations(_); "operations")]
    #[test_case("fn deeper(n) { deeper(n + 1) } deeper(0);" => matches EvalAltResult::ErrorStackOverflow(_); "call_levels")]
    #[test_case(DEEPER_THAN_LIMIT => matches EvalAltResult::ErrorParsing(ParseErrorType::ExprTooDeep, _); "expr_depth")]
    #[test_case(r#"let s = "ab"; loop { s += s; }"# => matches EvalAltResult::ErrorDataTooLarge(..); "string_size")]
    #[test_case("let a = []; loop { a.push(1); }" => matches EvalAltResult::ErrorDataTooLarge(..); "array_size")]
    #[test_case(r#"let m = #{}; let i = 0; loop { m.set("k" + i, i); i += 1; }"# => matches EvalAltResult::ErrorDataTooLarge(..); "map_size")]
    fn every_limit_stops_the_script(source: &str) -> EvalAltResult {
        failure(&restricted_engine(&LIMITS, HINT), source)
    }
}
