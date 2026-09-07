//! The `Language -> tags.scm` table.
//!
//! Every query is compiled into the binary. There is no runtime asset path, no override
//! environment variable, and no per-project query directory: the extractor a build ships with is
//! the extractor it runs.

use crate::Language;

/// Returns the vendored tags query source for a language.
///
/// TypeScript and JavaScript share a query because they share a grammar. Bazel's three file shapes
/// share Starlark's for the same reason.
pub(crate) const fn source(language: Language) -> &'static str {
    match language {
        Language::Rust => include_str!("../queries/rust/tags.scm"),
        Language::Python => include_str!("../queries/python/tags.scm"),
        Language::TypeScript | Language::JavaScript => {
            include_str!("../queries/typescript/tags.scm")
        }
        Language::Gleam => include_str!("../queries/gleam/tags.scm"),
        Language::Go => include_str!("../queries/go/tags.scm"),
        Language::Html => include_str!("../queries/html/tags.scm"),
        Language::Java => include_str!("../queries/java/tags.scm"),
        Language::C => include_str!("../queries/c/tags.scm"),
        Language::Cpp => include_str!("../queries/cpp/tags.scm"),
        Language::CSharp => include_str!("../queries/c_sharp/tags.scm"),
        Language::Ruby => include_str!("../queries/ruby/tags.scm"),
        Language::Php => include_str!("../queries/php/tags.scm"),
        Language::Swift => include_str!("../queries/swift/tags.scm"),
        Language::Kotlin => include_str!("../queries/kotlin/tags.scm"),
        Language::Scala => include_str!("../queries/scala/tags.scm"),
        Language::Bash => include_str!("../queries/bash/tags.scm"),
        Language::Lua => include_str!("../queries/lua/tags.scm"),
        Language::Elixir => include_str!("../queries/elixir/tags.scm"),
        Language::Markdown => include_str!("../queries/markdown/tags.scm"),
        Language::BazelBuild | Language::BazelModule | Language::BazelBzl => {
            include_str!("../queries/starlark/tags.scm")
        }
        Language::Zig => include_str!("../queries/zig/tags.scm"),
        Language::Nix => include_str!("../queries/nix/tags.scm"),
        Language::Dart => include_str!("../queries/dart/tags.scm"),
        Language::Toml => include_str!("../queries/toml/tags.scm"),
        Language::Yaml => include_str!("../queries/yaml/tags.scm"),
        Language::Sql => include_str!("../queries/sql/tags.scm"),
        Language::Css => include_str!("../queries/css/tags.scm"),
        Language::Json => include_str!("../queries/json/tags.scm"),
        Language::Hcl => include_str!("../queries/hcl/tags.scm"),
        Language::Containerfile => include_str!("../queries/containerfile/tags.scm"),
        Language::Make => include_str!("../queries/make/tags.scm"),
    }
}
