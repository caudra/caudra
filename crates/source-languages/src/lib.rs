//! Language detection, tree-sitter grammar selection, and the vendored tags queries that drive
//! symbol and reference extraction.
//!
//! This crate is the single `extension -> {grammar, tags query}` table. Two consumers share it: the
//! filesystem `index` tool, which needs detection and a grammar, and the code map, which
//! additionally needs the tags query and the capture-role mapping. Keeping one table is what stops
//! the two from disagreeing about what a `.tf` file is.
//!
//! Queries are compiled into the binary with `include_str!`. Nothing is read from disk or the
//! environment at run time, so a deployment cannot be handed a different extractor than the one it
//! was built with.

mod queries;
mod roles;

use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

pub use roles::{CaptureRole, ReferenceKind, SymbolKind};
use tree_sitter::Query;

/// A language Workcell can parse.
///
/// Bazel's three file shapes and TypeScript/JavaScript are separate variants even though each pair
/// shares a grammar: detection has to name what it found, and the config lanes need to be
/// distinguishable from the code lanes when resolution asks whether two symbols can refer to each
/// other.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Gleam,
    Go,
    Html,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Swift,
    Kotlin,
    Scala,
    Bash,
    Lua,
    Elixir,
    Markdown,
    BazelBuild,
    BazelModule,
    BazelBzl,
    Zig,
    Nix,
    Dart,
    Toml,
    Yaml,
    Sql,
    Css,
    Json,
    Hcl,
    Containerfile,
    Make,
}

/// What a language contributes to the graph.
///
/// The distinction is load-bearing at resolution: a `Config` key named `build` must never resolve a
/// `Code` function named `build`, and config lanes contribute no call edges at all. Ripwire calls
/// this `langCompatible`; the same rule is expressed here as a family comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LanguageFamily {
    /// Call graphs are meaningful. Definitions and references both participate.
    Code,
    /// Structured data. Keys become navigable sections; no call edges are ever emitted.
    Config,
    /// Prose and markup. Headings and elements become sections; no call edges.
    Prose,
}

/// The complete language table, in a fixed order.
///
/// The order is the declaration order of the enum and is what every "all languages" test iterates,
/// so a language added to the enum without a row here fails `covers_every_language`.
pub const ALL: &[Language] = &[
    Language::Rust,
    Language::Python,
    Language::TypeScript,
    Language::JavaScript,
    Language::Gleam,
    Language::Go,
    Language::Html,
    Language::Java,
    Language::C,
    Language::Cpp,
    Language::CSharp,
    Language::Ruby,
    Language::Php,
    Language::Swift,
    Language::Kotlin,
    Language::Scala,
    Language::Bash,
    Language::Lua,
    Language::Elixir,
    Language::Markdown,
    Language::BazelBuild,
    Language::BazelModule,
    Language::BazelBzl,
    Language::Zig,
    Language::Nix,
    Language::Dart,
    Language::Toml,
    Language::Yaml,
    Language::Sql,
    Language::Css,
    Language::Json,
    Language::Hcl,
    Language::Containerfile,
    Language::Make,
];

impl Language {
    /// Resolves a language from a path, by exact filename first and extension second.
    ///
    /// Returns `None` for anything unrecognized. Callers decide whether that is an error (the
    /// `index` tool refuses) or a skip that gets counted and disclosed (the code map crawl).
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let filename = path.file_name().and_then(|value| value.to_str())?;
        let exact = match filename {
            "MODULE.bazel" => Some(Self::BazelModule),
            "BUILD" | "BUILD.bazel" => Some(Self::BazelBuild),
            "Containerfile" | "Dockerfile" => Some(Self::Containerfile),
            "GNUmakefile" | "Makefile" => Some(Self::Make),
            _ => None,
        };
        if exact.is_some() {
            return exact;
        }
        Self::from_extension(path.extension().and_then(|value| value.to_str())?)
    }

    /// Resolves a language from a bare extension, without the leading dot.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        let language = match extension {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "ts" | "tsx" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "gleam" => Self::Gleam,
            "go" => Self::Go,
            "htm" | "html" => Self::Html,
            "java" => Self::Java,
            "c" | "h" => Self::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" | "ixx" => Self::Cpp,
            "cs" => Self::CSharp,
            "rb" | "rake" | "gemspec" => Self::Ruby,
            "php" => Self::Php,
            "swift" => Self::Swift,
            "kt" | "kts" => Self::Kotlin,
            "scala" | "sc" => Self::Scala,
            "sh" | "bash" | "zsh" => Self::Bash,
            "lua" => Self::Lua,
            "ex" | "exs" => Self::Elixir,
            "md" | "markdown" => Self::Markdown,
            "bzl" => Self::BazelBzl,
            "zig" => Self::Zig,
            "nix" => Self::Nix,
            "dart" => Self::Dart,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "sql" => Self::Sql,
            "css" => Self::Css,
            "json" => Self::Json,
            "hcl" | "tf" | "tfvars" => Self::Hcl,
            "dockerfile" => Self::Containerfile,
            "mk" => Self::Make,
            _ => return None,
        };
        Some(language)
    }

    /// The stable identifier reported in tool output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Gleam => "gleam",
            Self::Go => "go",
            Self::Html => "html",
            Self::Java => "java",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "c_sharp",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Swift => "swift",
            Self::Kotlin => "kotlin",
            Self::Scala => "scala",
            Self::Bash => "bash",
            Self::Lua => "lua_lang",
            Self::Elixir => "elixir",
            Self::Markdown => "markdown",
            Self::BazelBuild => "bazel_build",
            Self::BazelModule => "bazel_module",
            Self::BazelBzl => "bazel_bzl",
            Self::Zig => "zig",
            Self::Nix => "nix",
            Self::Dart => "dart",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Sql => "sql",
            Self::Css => "css",
            Self::Json => "json",
            Self::Hcl => "hcl",
            Self::Containerfile => "containerfile",
            Self::Make => "make",
        }
    }

    /// The tree-sitter grammar for this language.
    ///
    /// JavaScript is deliberately parsed with the TypeScript grammar: it is a superset for every
    /// construct extraction cares about, and using one grammar keeps a `.js` and a `.ts` file
    /// producing the same node kinds, which is what lets them share one tags query.
    #[must_use]
    pub fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript | Self::JavaScript => {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
            Self::Gleam => tree_sitter_gleam::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Elixir => tree_sitter_elixir::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
            Self::BazelBuild | Self::BazelModule | Self::BazelBzl => {
                tree_sitter_starlark::LANGUAGE.into()
            }
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            Self::Nix => tree_sitter_nix::LANGUAGE.into(),
            Self::Dart => tree_sitter_dart::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Self::Sql => tree_sitter_sequel::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Hcl => tree_sitter_hcl::LANGUAGE.into(),
            Self::Containerfile => tree_sitter_containerfile::LANGUAGE.into(),
            Self::Make => tree_sitter_make::LANGUAGE.into(),
        }
    }

    /// Whether this language contributes call edges, config sections, or prose sections.
    #[must_use]
    pub const fn family(self) -> LanguageFamily {
        match self {
            Self::Rust
            | Self::Python
            | Self::TypeScript
            | Self::JavaScript
            | Self::Gleam
            | Self::Go
            | Self::Java
            | Self::C
            | Self::Cpp
            | Self::CSharp
            | Self::Ruby
            | Self::Php
            | Self::Swift
            | Self::Kotlin
            | Self::Scala
            | Self::Bash
            | Self::Lua
            | Self::Elixir
            | Self::Zig
            | Self::Dart
            | Self::Sql
            | Self::BazelBuild
            | Self::BazelModule
            | Self::BazelBzl
            | Self::Make => LanguageFamily::Code,
            Self::Toml | Self::Yaml | Self::Json | Self::Hcl | Self::Nix | Self::Containerfile => {
                LanguageFamily::Config
            }
            Self::Markdown | Self::Html | Self::Css => LanguageFamily::Prose,
        }
    }

    /// Whether a reference written in `self` may resolve to a definition written in `other`.
    ///
    /// Same-language is always compatible. TypeScript and JavaScript are mutually compatible
    /// because they share a grammar, a module system, and in practice a single project. Everything
    /// else is refused: a YAML key named `deploy` and a Go function named `deploy` are not the same
    /// symbol, and letting one resolve the other manufactures edges out of a spelling coincidence.
    #[must_use]
    pub const fn compatible_with(self, other: Self) -> bool {
        matches!(
            (self, other),
            (
                Self::TypeScript | Self::JavaScript,
                Self::TypeScript | Self::JavaScript
            )
        ) || matches!(
            (self, other),
            (
                Self::BazelBuild | Self::BazelModule | Self::BazelBzl,
                Self::BazelBuild | Self::BazelModule | Self::BazelBzl
            )
        ) || (self as u8) == (other as u8)
    }

    /// The uncompiled tags query source for this language.
    #[must_use]
    pub const fn tags_source(self) -> &'static str {
        queries::source(self)
    }

    /// The compiled tags query, built once per language for the life of the process.
    ///
    /// Compilation is not free and every file of a given language reuses the same query, so a
    /// whole-repository crawl pays for each language once rather than once per file.
    ///
    /// # Panics
    ///
    /// Panics if the vendored query does not compile against the vendored grammar. That pairing is
    /// fixed at build time and asserted by `every_query_compiles`, so a panic here means the two
    /// were updated out of step, which no run-time fallback could paper over honestly.
    #[must_use]
    pub fn tags_query(self) -> &'static Query {
        static COMPILED: OnceLock<Mutex<Vec<Option<&'static Query>>>> = OnceLock::new();
        let cache = COMPILED.get_or_init(|| Mutex::new(vec![None; ALL.len()]));
        let index = self as usize;
        let mut guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(query) = guard[index] {
            return query;
        }
        let compiled: &'static Query = Box::leak(Box::new(
            Query::new(&self.grammar(), self.tags_source())
                .unwrap_or_else(|error| panic!("{} tags query: {error}", self.name())),
        ));
        guard[index] = Some(compiled);
        compiled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_compiles_against_its_grammar() {
        // This is the grammar-bump canary. A crates.io grammar update that renames or removes a
        // node kind fails here with the offending language named, instead of silently extracting
        // nothing from every file of that language.
        for &language in ALL {
            let _ = language.tags_query();
        }
    }

    #[test]
    fn all_lists_every_language_exactly_once() {
        let mut seen = ALL.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ALL.len(), "ALL contains a duplicate");
        for (index, &language) in ALL.iter().enumerate() {
            assert_eq!(
                language as usize,
                index,
                "{} is out of declaration order in ALL",
                language.name()
            );
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = ALL.iter().map(|language| language.name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn detection_prefers_exact_filenames_over_extensions() {
        assert_eq!(
            Language::from_path(Path::new("a/BUILD.bazel")),
            Some(Language::BazelBuild)
        );
        assert_eq!(
            Language::from_path(Path::new("a/Dockerfile")),
            Some(Language::Containerfile)
        );
        assert_eq!(
            Language::from_path(Path::new("a/main.rs")),
            Some(Language::Rust)
        );
        assert_eq!(Language::from_path(Path::new("a/README")), None);
        assert_eq!(Language::from_path(Path::new("a/thing.unknown")), None);
    }

    #[test]
    fn config_and_prose_lanes_never_claim_code_compatibility() {
        // The spelling-coincidence guard: a config key and a code symbol with the same name are
        // not the same symbol, and resolution must never join them.
        assert!(!Language::Yaml.compatible_with(Language::Go));
        assert!(!Language::Json.compatible_with(Language::TypeScript));
        assert!(!Language::Markdown.compatible_with(Language::Rust));
        assert!(Language::Yaml.compatible_with(Language::Yaml));
    }

    #[test]
    fn typescript_and_javascript_resolve_each_other() {
        assert!(Language::TypeScript.compatible_with(Language::JavaScript));
        assert!(Language::JavaScript.compatible_with(Language::TypeScript));
        assert!(Language::BazelBuild.compatible_with(Language::BazelBzl));
    }
}
