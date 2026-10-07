//! Prints the parse tree and tags-query captures for a source file.
//!
//! Authoring a `tags.scm` pattern requires knowing the exact node kinds and field names a grammar
//! produces, which differ between grammar versions and are not documented anywhere authoritative.
//! This is the tool that answers that question against the grammar actually vendored here.
//!
//! ```text
//! cargo run -p workcell-source-languages --example dump -- path/to/file.rs
//! cargo run -p workcell-source-languages --example dump -- path/to/file.rs --captures
//! ```

use std::{path::Path, process::ExitCode};

use tree_sitter::StreamingIterator as _;
use workcell_source_languages::{CaptureRole, Language};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = arguments.first() else {
        eprintln!("usage: dump <path> [--captures]");
        return ExitCode::FAILURE;
    };
    let captures_only = arguments.iter().any(|argument| argument == "--captures");
    let path = Path::new(path);
    let Some(language) = Language::from_path(path) else {
        eprintln!("no language for {}", path.display());
        return ExitCode::FAILURE;
    };
    let Ok(source) = std::fs::read_to_string(path) else {
        eprintln!("cannot read {}", path.display());
        return ExitCode::FAILURE;
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language.grammar()).is_err() {
        eprintln!("cannot load the {} grammar", language.name());
        return ExitCode::FAILURE;
    }
    let Some(tree) = parser.parse(&source, None) else {
        eprintln!("parse failed");
        return ExitCode::FAILURE;
    };

    println!("language: {}", language.name());
    if !captures_only {
        print_tree(tree.root_node(), &source, 0);
        println!("---");
    }

    let query = language.tags_query();
    let names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    let mut total = 0usize;
    while let Some(matched) = matches.next() {
        for capture in matched.captures() {
            let name = names[capture.index as usize];
            let role = CaptureRole::classify(name);
            let text = capture.node.utf8_text(source.as_bytes()).unwrap_or("<?>");
            let line = capture.node.start_position().row + 1;
            println!("{line:>5}  @{name:<24} {role:?}  {text:?}");
            total += 1;
        }
    }
    println!("--- {total} captures");
    ExitCode::SUCCESS
}

fn print_tree(node: tree_sitter::Node<'_>, source: &str, depth: usize) {
    if depth > 40 {
        return;
    }
    let indent = "  ".repeat(depth);
    let text = node.utf8_text(source.as_bytes()).unwrap_or("");
    let preview: String = text.chars().take(48).collect();
    let preview = preview.replace('\n', "\\n");
    println!(
        "{indent}{}{} [{}] {preview:?}",
        if node.is_named() { "" } else { "\"" },
        node.kind(),
        node.start_position().row + 1
    );
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        print_tree(child, source, depth + 1);
    }
}
