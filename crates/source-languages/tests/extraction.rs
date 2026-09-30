//! The gate that a vendored query actually extracts something.
//!
//! `Query::new` succeeding proves only that every node kind named in a pattern exists in the
//! grammar. It does not prove the pattern matches anything a real file contains, and a query that
//! compiles and captures nothing is the silent failure this suite exists to prevent: the map would
//! render, rank, and disclose no truncation while knowing nothing about that language.
//!
//! Each language owns one fixture under `tests/fixtures/`, named so `Language::from_path` resolves
//! it. The harness discovers them by that rule, so adding a language means adding a query and a
//! fixture, never editing this file.

use std::{collections::BTreeMap, path::Path};

use test_case::test_case;
use tree_sitter::StreamingIterator as _;
use workcell_source_languages::{ALL, CaptureRole, Language, LanguageFamily, ReferenceKind};

#[derive(Default)]
struct Captured {
    definitions: BTreeMap<String, usize>,
    references: BTreeMap<String, usize>,
    named_definitions: usize,
    named_references: usize,
    unnamed_definitions: usize,
}

fn fixture_for(language: Language) -> (std::path::PathBuf, String) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let entries = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
    let mut found = Vec::new();
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if Language::from_path(&path) == Some(language) {
            found.push(path);
        }
    }
    found.sort();
    assert_eq!(
        found.len(),
        1,
        "{} needs exactly one fixture in tests/fixtures, found {found:?}",
        language.name()
    );
    let path = found.remove(0);
    let source = std::fs::read_to_string(&path).expect("fixture is UTF-8");
    (path, source)
}

fn capture(language: Language, source: &str) -> Captured {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.grammar())
        .expect("grammar loads");
    let tree = parser.parse(source, None).expect("fixture parses");
    let query = language.tags_query();
    let names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    let mut captured = Captured::default();
    while let Some(matched) = matches.next() {
        let mut anchor = None;
        let mut name = None;
        for entry in matched.captures() {
            let role = CaptureRole::classify(names[entry.index as usize]);
            match role {
                CaptureRole::Definition(_) | CaptureRole::Reference(_) => anchor = Some(role),
                CaptureRole::Name => {
                    name = entry
                        .node
                        .utf8_text(source.as_bytes())
                        .ok()
                        .map(str::to_owned);
                }
                _ => {}
            }
        }
        match anchor {
            Some(CaptureRole::Definition(kind)) => match name {
                Some(name) => {
                    captured.named_definitions += 1;
                    *captured
                        .definitions
                        .entry(format!("{}:{name}", kind.name()))
                        .or_default() += 1;
                }
                None => captured.unnamed_definitions += 1,
            },
            Some(CaptureRole::Reference(kind)) => {
                if let Some(name) = name {
                    captured.named_references += 1;
                    *captured
                        .references
                        .entry(format!("{}:{name}", kind.name()))
                        .or_default() += 1;
                }
            }
            _ => {}
        }
    }
    captured
}

#[test]
fn every_language_extracts_definitions_from_its_fixture() {
    let mut empty = Vec::new();
    for &language in ALL {
        let (path, source) = fixture_for(language);
        let captured = capture(language, &source);
        if captured.named_definitions == 0 {
            empty.push(format!(
                "{} ({}) captured no named definition",
                language.name(),
                path.display()
            ));
        }
    }
    assert!(empty.is_empty(), "{}", empty.join("\n"));
}

#[test]
fn every_code_language_extracts_call_references() {
    // Config and prose lanes are data: they contribute sections and no call edges by design, so
    // only the code family is held to this. A code language with no call pattern would produce an
    // edgeless graph, which ranks every symbol identically and looks like a working map.
    let mut empty = Vec::new();
    for &language in ALL {
        if language.family() != LanguageFamily::Code {
            continue;
        }
        let (path, source) = fixture_for(language);
        let captured = capture(language, &source);
        let calls = captured
            .references
            .iter()
            .filter(|(key, _)| key.starts_with(ReferenceKind::Call.name()))
            .count();
        if calls == 0 {
            empty.push(format!(
                "{} ({}) captured no call reference",
                language.name(),
                path.display()
            ));
        }
    }
    assert!(empty.is_empty(), "{}", empty.join("\n"));
}

#[test]
fn config_and_prose_lanes_emit_no_call_references() {
    // The other half of the lane rule. A config key is not a call site, and an edge minted from one
    // would assert control flow that no execution performs.
    let mut offending = Vec::new();
    for &language in ALL {
        if language.family() == LanguageFamily::Code {
            continue;
        }
        let (path, source) = fixture_for(language);
        let captured = capture(language, &source);
        for key in captured.references.keys() {
            if key.starts_with(ReferenceKind::Call.name()) {
                offending.push(format!(
                    "{} ({}) emitted a call reference {key}",
                    language.name(),
                    path.display()
                ));
            }
        }
    }
    assert!(offending.is_empty(), "{}", offending.join("\n"));
}

#[test]
fn every_definition_carries_a_name() {
    // A definition capture without an accompanying `@name` cannot be addressed, ranked by name, or
    // resolved against, so it is a pattern bug rather than a partial result.
    let mut offending = Vec::new();
    for &language in ALL {
        let (path, source) = fixture_for(language);
        let captured = capture(language, &source);
        if captured.unnamed_definitions > 0 {
            offending.push(format!(
                "{} ({}) produced {} definition captures with no @name",
                language.name(),
                path.display(),
                captured.unnamed_definitions
            ));
        }
    }
    assert!(offending.is_empty(), "{}", offending.join("\n"));
}

#[test_case("#define LIMIT 4\n", "macro:LIMIT" ; "an object like macro")]
#[test_case("struct node { TAILQ_ENTRY(node) link; };\n", "field:link" ; "a field typed by a one argument macro")]
#[test_case("struct list { TAILQ_HEAD(head, node) first; };\n", "field:first" ; "a field typed by a two argument macro")]
#[test_case("void set_callback(item *i, int (*callback)(item *i));\n", "function:set_callback" ; "a prototype the grammar reads as a variable")]
fn a_c_header_parsed_as_cpp_keeps_its_definitions(source: &str, expected: &str) {
    // Every `.h` parses as C++, so the C++ query has to recover what the C grammar parses natively.
    let captured = capture(Language::Cpp, source);
    assert!(
        captured.definitions.contains_key(expected),
        "missing {expected}; captured {:?}",
        captured.definitions
    );
}

#[test_case("using TokenId = int;\n", "class:TokenId" ; "a type alias")]
#[test_case("template <typename T> using Rows = std::vector<T>;\n", "class:Rows" ; "an alias template")]
fn a_cpp_type_alias_is_a_type_definition(source: &str, expected: &str) {
    let captured = capture(Language::Cpp, source);
    assert!(
        captured.definitions.contains_key(expected),
        "missing {expected}; captured {:?}",
        captured.definitions
    );
}

#[test_case("__global__ void scale(float* data) {}\n", "function:scale" ; "a kernel")]
#[test_case("__device__ __forceinline__ float half_of(float v) { return v / 2; }\n", "function:half_of" ; "a device function")]
#[test_case("__constant__ float table[64];\n", "constant:table" ; "a constant memory table")]
#[test_case("void run(float* d) { scale<<<1, 32>>>(d); }\n", "call:scale" ; "a kernel launch")]
#[test_case("void run(float* d) { scale<32><<<1, 32>>>(d); }\n", "call:scale" ; "a templated kernel launch")]
fn cuda_parses_under_the_shared_cpp_query(source: &str, expected: &str) {
    let captured = capture(Language::Cuda, source);
    assert!(
        captured.definitions.contains_key(expected) || captured.references.contains_key(expected),
        "missing {expected}; captured {:?} and {:?}",
        captured.definitions,
        captured.references
    );
}

#[test_case("@interface Canvas : NSObject\n@end\n", "class:Canvas" ; "an interface")]
#[test_case("@implementation Canvas (Drawing)\n@end\n", "class:Canvas" ; "a category names the class it extends")]
#[test_case("@protocol Drawable\n- (void)draw;\n@end\n", "interface:Drawable" ; "a protocol")]
#[test_case("@protocol P\n- (id)initWithTitle:(id)t count:(int)c;\n@end\n", "method:initWithTitle" ; "a method named by its first keyword")]
#[test_case("@interface Canvas : NSObject\n@property NSString *title;\n@end\n", "field:title" ; "a property")]
#[test_case("static int clamp(int v) { return v; }\n", "function:clamp" ; "a c function")]
#[test_case("void f(id c) { [c drawInContext:nil]; }\n", "call:drawInContext" ; "a message send")]
#[test_case("@import CoreGraphics;\n", "import:CoreGraphics" ; "a module import")]
fn objc_captures_its_layer_on_top_of_c(source: &str, expected: &str) {
    let captured = capture(Language::ObjC, source);
    assert!(
        captured.definitions.contains_key(expected) || captured.references.contains_key(expected),
        "missing {expected}; captured {:?} and {:?}",
        captured.definitions,
        captured.references
    );
}

#[test]
fn objc_mints_one_call_reference_per_call_site() {
    // The C patterns already capture C calls, so a repeated pattern in the Objective-C layer would
    // double every edge, and a selector's later keywords are not separate methods.
    let captured = capture(
        Language::ObjC,
        "void f(id c) { clamp(1); [c initWithTitle:@\"a\" count:1]; }\n",
    );
    assert_eq!(captured.references.get("call:clamp"), Some(&1));
    assert_eq!(captured.references.get("call:initWithTitle"), Some(&1));
    assert_eq!(captured.references.get("call:count"), None);
}

#[test_case("struct Widget { Widget(int, char) stray; };\n", "field:stray" ; "a constructor followed by a stray token")]
#[test_case("void set_callback(item *i, int (*callback)(item *i));\n", "constant:set_callback" ; "a prototype the grammar reads as a variable")]
fn cpp_header_recovery_mints_no_false_definition(source: &str, unexpected: &str) {
    let captured = capture(Language::Cpp, source);
    assert!(
        !captured.definitions.contains_key(unexpected),
        "minted {unexpected}; captured {:?}",
        captured.definitions
    );
}

#[test]
fn rust_captures_the_exact_shape_the_span_fix_depends_on() {
    // Ripwire's most expensive query bug: `@definition.method` captured on the `declaration_list`
    // wrapper, which has no `body:` field, so span widening climbed to the whole `impl` block and
    // every method inherited it. Enclosing attribution then resolved every call in the block to one
    // arbitrary method, which is how `Self::helper()` inside `bump` became `helper` calling itself.
    // The discriminator is the pattern shape, so the capture belongs on the `function_item`.
    let source = r"
pub struct Widget { pub count: u32 }
impl Widget {
    pub fn bump(&mut self) -> u32 { Self::helper(self.count) }
    fn helper(value: u32) -> u32 { value + 1 }
}
";
    let captured = capture(Language::Rust, source);
    assert_eq!(captured.definitions.get("method:bump"), Some(&1));
    assert_eq!(captured.definitions.get("method:helper"), Some(&1));
    assert_eq!(captured.definitions.get("class:Widget"), Some(&1));
    assert_eq!(captured.references.get("call:helper"), Some(&1));

    // The span the method capture reports must be the function, not the enclosing block.
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&Language::Rust.grammar())
        .expect("grammar loads");
    let tree = parser.parse(source, None).expect("parses");
    let query = Language::Rust.tags_query();
    let names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    let mut method_spans = Vec::new();
    while let Some(matched) = matches.next() {
        for entry in matched.captures() {
            if names[entry.index as usize] == "definition.method" {
                method_spans.push(
                    entry
                        .node
                        .utf8_text(source.as_bytes())
                        .expect("utf8")
                        .to_owned(),
                );
            }
        }
    }
    assert_eq!(method_spans.len(), 2);
    for span in &method_spans {
        assert!(
            span.starts_with("pub fn bump") || span.starts_with("fn helper"),
            "method span widened past the function: {span:?}"
        );
        assert!(
            !span.contains("impl Widget"),
            "method span swallowed the impl block: {span:?}"
        );
    }
}

#[test]
fn rust_captures_qualified_and_turbofish_calls() {
    // The dominant Rust call form is a `::` path. Upstream tree-sitter-rust's tags query has no
    // pattern for it, so a stock query is blind to most of a Rust codebase's call graph.
    let source = r"
fn caller() {
    Widget::new();
    util::deep::deepfn();
    Self::helper();
    Vec::<u32>::new();
    generic::<u32>(1);
    plain();
    make_it!();
}
";
    let captured = capture(Language::Rust, source);
    for expected in [
        "call:new",
        "call:deepfn",
        "call:helper",
        "call:generic",
        "call:plain",
        "call:make_it",
    ] {
        assert!(
            captured.references.contains_key(expected),
            "missing {expected}; captured {:?}",
            captured.references
        );
    }
}
