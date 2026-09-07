//! The facts extraction produces and every later stage reads.
//!
//! These are plain data. Nothing here knows about the filesystem, a protocol, or a ranking, which
//! is what lets each stage be tested on hand-built inputs rather than a real tree.

use workcell_source_languages::{Language, ReferenceKind, SymbolKind};

/// An index into the sorted file list.
///
/// Files are numbered after the crawl sorts them by byte, so the same tree produces the same ids on
/// every run regardless of directory iteration order or how the parse work was scheduled.
pub type FileId = u32;

/// An index into the symbol table.
///
/// Not a pointer. Ids are assigned in `(file_id, byte_start)` order after extraction has been
/// merged and re-sorted, so they are stable across runs and usable as a deterministic tiebreak when
/// two symbols score identically.
pub type NodeId = u32;

/// A half-open byte range within one file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteSpan {
    pub start: usize,
    pub end: usize,
}

impl ByteSpan {
    #[must_use]
    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.end <= self.start
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// An inclusive 1-based line range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
}

/// One file admitted to the crawl.
#[derive(Clone, Debug)]
pub struct SourceFile {
    pub id: FileId,
    /// Root-relative, forward-slash separated. Never an absolute path: nothing downstream should be
    /// able to reconstruct where the root lives.
    pub path: String,
    pub language: Language,
    pub bytes: usize,
    pub lines: usize,
}

impl SourceFile {
    /// The directory portion of the path, or an empty string at the root.
    ///
    /// Used by resolution tier two, which prefers a definition in the same directory.
    #[must_use]
    pub fn directory(&self) -> &str {
        match self.path.rfind('/') {
            Some(index) => &self.path[..index],
            None => "",
        }
    }
}

/// Structural metrics for one definition.
///
/// Every field is a count of something present in the syntax. None of them is an estimate, and none
/// of them is comparable across languages without care, which is why the renderer labels them
/// rather than combining them into a single score.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Metrics {
    /// Cyclomatic complexity: one plus the number of branch points in the body.
    pub complexity: u32,
    /// Physical lines the definition spans.
    pub lines: u32,
    /// Declared parameters.
    pub parameters: u32,
    /// Deepest nesting of block-introducing constructs inside the body.
    pub max_nesting: u32,
}

/// One definition extracted from one file.
#[derive(Clone, Debug)]
pub struct Definition {
    pub node: NodeId,
    pub file: FileId,
    pub name: String,
    pub kind: SymbolKind,
    /// Byte offset of the name token. Half of the dedup identity.
    pub name_start: usize,
    /// The definition's own span: signature through closing delimiter.
    pub span: ByteSpan,
    pub lines: LineSpan,
    pub metrics: Metrics,
    /// Whether the definition sits inside test scaffolding.
    ///
    /// Determined syntactically from the enclosing constructs, never from the file path. A helper
    /// in `#[cfg(test)] mod tests` inside a production file is test scaffolding; a production
    /// function in a file that happens to live under `tests/` is not.
    pub test_scope: bool,
    /// A doc comment attached to the definition, already trimmed of comment markers.
    pub documentation: Option<String>,
}

/// One unresolved use site.
///
/// Resolution turns these into edges. Until then a reference knows only the name it spelled, not
/// what that name means.
#[derive(Clone, Debug)]
pub struct Reference {
    pub file: FileId,
    pub name: String,
    pub kind: ReferenceKind,
    /// The leading segment of a qualified reference, when the syntax supplies one.
    ///
    /// `Widget::new()` records `Widget`. The precise resolution tiers use it; the base ladder
    /// ignores it.
    pub qualifier: Option<String>,
    pub byte: usize,
    pub line: usize,
    /// The definition this reference sits inside, if any.
    ///
    /// A reference at file scope has none. Attribution is by innermost enclosing span, which is why
    /// a definition capture landing on a wrapper node corrupts every reference in the block.
    pub enclosing: Option<NodeId>,
}

/// Why a file the crawl saw is not represented in the graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// No grammar covers the extension.
    UnknownLanguage,
    /// Larger than the configured per-file ceiling.
    Oversize,
    /// Contains a NUL byte in its leading sample.
    Binary,
    /// Not valid UTF-8.
    Encoding,
    /// The parser did not finish inside its deadline.
    ParserTimeout,
    /// Admitted and parsed, but the tree came back with errors covering the whole file.
    Unparseable,
}

impl SkipReason {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::UnknownLanguage => "unknown-language",
            Self::Oversize => "oversize",
            Self::Binary => "binary",
            Self::Encoding => "encoding",
            Self::ParserTimeout => "parser-timeout",
            Self::Unparseable => "unparseable",
        }
    }
}

/// A file the crawl declined to index, with the reason.
///
/// Kept rather than discarded: a map that quietly omits half a repository looks identical to one
/// that covered it, and the caller cannot tell which they received.
#[derive(Clone, Debug)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
    /// The extension, when the skip was about the extension. Aggregated into the `unindexed`
    /// disclosure so a caller sees `ml:793` rather than 793 individual paths.
    pub extension: Option<String>,
}

/// Everything extraction produced for one tree.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub files: Vec<SourceFile>,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    pub skipped: Vec<SkippedFile>,
    /// Files whose parse produced an error node somewhere.
    ///
    /// These are still indexed. A syntax error in one function does not invalidate the rest of the
    /// file, but the count is disclosed so a caller can weigh it.
    pub files_with_parse_errors: usize,
}

impl Facts {
    /// Looks up a definition by node id.
    ///
    /// Ids are dense indices assigned in sorted order, so this is a bounds-checked index rather
    /// than a search.
    #[must_use]
    pub fn definition(&self, node: NodeId) -> Option<&Definition> {
        self.definitions.get(node as usize)
    }

    #[must_use]
    pub fn file(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(id as usize)
    }

    /// The definition's path, for rendering.
    #[must_use]
    pub fn path_of(&self, node: NodeId) -> Option<&str> {
        let definition = self.definition(node)?;
        Some(self.file(definition.file)?.path.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_is_the_path_prefix() {
        let file = |path: &str| SourceFile {
            id: 0,
            path: path.to_owned(),
            language: Language::Rust,
            bytes: 0,
            lines: 0,
        };
        assert_eq!(file("src/index/mod.rs").directory(), "src/index");
        assert_eq!(file("main.rs").directory(), "");
    }

    #[test]
    fn span_containment_is_inclusive_of_equal_bounds() {
        let outer = ByteSpan { start: 0, end: 10 };
        assert!(outer.contains(ByteSpan { start: 0, end: 10 }));
        assert!(outer.contains(ByteSpan { start: 2, end: 8 }));
        assert!(!outer.contains(ByteSpan { start: 2, end: 11 }));
    }
}
