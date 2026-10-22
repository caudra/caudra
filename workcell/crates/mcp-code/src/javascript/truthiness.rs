//! Which tests mean the same in both languages. JavaScript holds an empty list or object true and
//! Python false, so a test on a bare value could change which host calls run. A [`SafeTest`] is
//! the only thing a condition takes, and only [`safe_test`] makes one from a lowered value.

use super::expressions::{Fact, Lowered};
use super::python::{Expression, Logic};
use super::{Construct, Translation, Untranslated};

/// A test whose truthiness cannot differ between the two languages.
#[derive(Clone)]
pub(super) struct SafeTest(Expression);

impl SafeTest {
    pub(super) fn expression(&self) -> &Expression {
        &self.0
    }

    pub(super) fn into_expression(self) -> Expression {
        self.0
    }

    pub(super) fn not(test: Self) -> Self {
        Self(Expression::Not(Box::new(test)))
    }

    pub(super) fn logical(left: Self, operator: Logic, right: Self) -> Self {
        Self(Expression::Logical {
            left: Box::new(left),
            operator,
            right: Box::new(right),
        })
    }
}

/// `lowered` as a test, when it is a comparison, a boolean or a whole number, which is never NaN.
/// Anything else refuses, naming it as `written` on `line`.
pub(super) fn safe_test(lowered: Lowered, written: &str, line: usize) -> Translation<SafeTest> {
    match lowered.fact {
        Fact::Bool | Fact::Integer | Fact::Count => Ok(SafeTest(lowered.expression)),
        Fact::Unknown | Fact::Optional | Fact::Number | Fact::Str | Fact::Object => {
            Err(Untranslated {
                construct: Construct::Condition(written.to_owned()),
                line,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::super::tests::{python, refused};
    use super::super::{Construct, Untranslated};

    #[test_case("const r = await gh.get();\nif (r.items.length > 0 && r.name !== \"x\") { console.log(1); }", "r = await gh.get()\nif len(r[\"items\"]) > 0 and r[\"name\"] != \"x\":\n    print(1)" ; "comparisons")]
    #[test_case("const r = await gh.get();\nif (r.next == null || r.next != undefined) { console.log(1); }", "r = await gh.get()\nif r.get(\"next\") is None or r.get(\"next\") is not None:\n    print(1)" ; "null_checks")]
    #[test_case("let n = 0;\nwhile (true) { n++; if (n > 2) break; }", "n = 0\nwhile True:\n    n += 1\n    if n > 2:\n        break" ; "boolean_literal")]
    #[test_case("const r = await gh.get();\nif (r.items.length) { console.log(1); }", "r = await gh.get()\nif len(r[\"items\"]):\n    print(1)" ; "length")]
    #[test_case("const xs = [1];\nconst n = xs.length * 2;\nif (n) { console.log(n); }", "xs = [1]\nn = len(xs) * 2\nif n:\n    print(n)" ; "number")]
    #[test_case("let n = 0;\nn += 1;\nif (n) { console.log(n); }", "n = 0\nn += 1\nif n:\n    print(n)" ; "let_whole_number")]
    #[test_case("const s = \"a\";\nif (s.includes(\"a\") || s.startsWith(\"b\") || s.endsWith(\"c\")) { console.log(1); }", "s = \"a\"\nif \"a\" in s or s.startswith(\"b\") or s.endswith(\"c\"):\n    print(1)" ; "string_predicates")]
    #[test_case("const xs = [\"a\"];\nif (!Array.isArray(xs) || xs.some(x => x.length > 1) || xs.every(x => x.length > 0)) { console.log(1); }", "xs = [\"a\"]\nif not isinstance(xs, list) or any(len(x) > 1 for x in xs) or all(len(x) > 0 for x in xs):\n    print(1)" ; "boolean_calls_and_not")]
    #[test_case("const r = await gh.get();\nconst a = r.items.length > 0 ? r.items : [];", "r = await gh.get()\na = r[\"items\"] if len(r[\"items\"]) > 0 else []" ; "ternary")]
    fn safe_test_translates(js: &str, expected: &str) {
        assert_eq!(python(js), expected);
    }

    #[test_case("const xs = [1];\nif (xs) { console.log(1); }", "xs", 2 ; "bare_array")]
    #[test_case("const r = await gh.get();\nif (!r.items) { console.log(1); }", "r.items", 2 ; "negated_member")]
    #[test_case("const r = await gh.get();\nif (r.items && r.items.length > 0) { console.log(1); }", "r.items", 2 ; "member_guarding_a_comparison")]
    #[test_case("const r = await gh.get();\nwhile (r.next) { console.log(1); }", "r.next", 2 ; "while_on_a_member")]
    #[test_case("const xs = [1];\nconst a = xs ? 1 : 2;", "xs", 2 ; "ternary_on_a_bare_value")]
    #[test_case("const r = await gh.get();\nconst a = r.items.filter(i => i.open);", "i.open", 2 ; "filter_predicate")]
    #[test_case("const n = parseFloat(\"1\");\nif (n) { console.log(n); }", "n", 2 ; "parsed_float")]
    #[test_case("const n = Number(\"1\");\nwhile (n) { console.log(n); }", "n", 2 ; "converted_number")]
    #[test_case("const xs = [1];\nconst n = xs.length / 2;\nif (n) { console.log(n); }", "n", 3 ; "quotient")]
    #[test_case("const r = await gh.get();\nconst a = r.items && r.items.length;", "r.items", 2 ; "and_over_a_member_as_a_value")]
    #[test_case("const s = \"a\";\nif (s) { console.log(s); }", "s", 2 ; "string")]
    #[test_case("const o = {};\nif (o) { console.log(1); }", "o", 2 ; "empty_object")]
    fn unsafe_test_refuses(js: &str, condition: &str, line: usize) {
        assert_eq!(
            refused(js),
            Untranslated {
                construct: Construct::Condition(condition.to_owned()),
                line,
            }
        );
    }
}
