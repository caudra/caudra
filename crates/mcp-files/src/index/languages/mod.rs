mod bash;
mod bazel;
mod c;
mod common;
mod containerfile;
mod cpp;
mod csharp;
mod css;
mod dart;
mod elixir;
mod gleam;
mod go;
mod hcl;
mod html;
mod java;
mod json;
mod kotlin;
mod lua;
mod make;
mod markdown;
mod nix;
mod php;
mod python;
mod ruby;
mod rust;
mod scala;
mod sql;
mod swift;
mod toml;
mod typescript;
mod yaml;
mod zig;

use tree_sitter::Node;

use super::{Language, model::ParsedSkeleton, traversal::Context};
use common::{ExtractResult, LanguageSpec, extract_default};

pub(super) fn extract(
    language: Language,
    root: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<ParsedSkeleton> {
    match language {
        Language::Html => html::extract(root, context),
        Language::Markdown => markdown::extract(root, context),
        Language::BazelBuild => bazel::extract_build(root, context),
        Language::BazelModule => bazel::extract_module(root, context),
        Language::BazelBzl => bazel::extract_bzl(root, context),
        Language::Nix => nix::extract(root, context),
        Language::Toml => toml::extract(root, context),
        Language::Yaml => yaml::extract(root, context),
        Language::Css => css::extract(root, context),
        Language::Json => json::extract(root, context),
        Language::Hcl => hcl::extract(root, context),
        Language::Containerfile => containerfile::extract(root, context),
        Language::Make => make::extract(root, context),
        _ => extract_default(root, context, spec(language)),
    }
}

fn spec(language: Language) -> LanguageSpec {
    match language {
        Language::Rust => rust::spec(),
        Language::Python => python::spec(),
        Language::TypeScript | Language::JavaScript => typescript::spec(),
        Language::Gleam => gleam::spec(),
        Language::Go => go::spec(),
        Language::Java => java::spec(),
        Language::C => c::spec(),
        Language::Cpp => cpp::spec(),
        Language::CSharp => csharp::spec(),
        Language::Ruby => ruby::spec(),
        Language::Php => php::spec(),
        Language::Swift => swift::spec(),
        Language::Kotlin => kotlin::spec(),
        Language::Scala => scala::spec(),
        Language::Bash => bash::spec(),
        Language::Lua => lua::spec(),
        Language::Elixir => elixir::spec(),
        Language::Zig => zig::spec(),
        Language::Dart => dart::spec(),
        Language::Sql => sql::spec(),
        Language::Html
        | Language::Markdown
        | Language::BazelBuild
        | Language::BazelModule
        | Language::BazelBzl
        | Language::Nix
        | Language::Toml
        | Language::Yaml
        | Language::Css
        | Language::Json
        | Language::Hcl
        | Language::Containerfile
        | Language::Make => unreachable!("custom extractor requested as a default extractor"),
    }
}
