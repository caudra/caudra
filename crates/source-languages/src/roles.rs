//! The capture-name to role mapping shared by every language.
//!
//! One query engine runs over all languages. A `tags.scm` file names what it found with a capture
//! (`@definition.function`, `@reference.call`, `@name`) and this table turns that name into a role.
//! Adding a language is a query file plus, at most, a row here.

/// What a definition capture found.
///
/// The ordering is a specificity ranking, not an arbitrary enum order: when two captures land on
/// the same name token, the higher-specificity kind wins. A method captured both as
/// `@definition.method` and `@definition.function` must record as a method.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SymbolKind {
    /// A key in a config lane or a heading in a prose lane. Never emits a call edge.
    Section,
    /// A free function.
    Function,
    /// A named constant or a module-level static.
    Constant,
    /// A field or property on a type.
    Field,
    /// A namespace, module, or package declaration.
    Module,
    /// A macro definition.
    Macro,
    /// A struct, class, enum, union, or type alias.
    Class,
    /// A trait, interface, or protocol.
    Interface,
    /// A function defined inside a type, trait, impl, or class body.
    Method,
}

impl SymbolKind {
    /// The stable identifier reported in tool output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Section => "section",
            Self::Function => "function",
            Self::Constant => "constant",
            Self::Field => "field",
            Self::Module => "module",
            Self::Macro => "macro",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Method => "method",
        }
    }

    /// Whether this kind can be the target of a call edge.
    ///
    /// Sections cannot: a YAML key is data, and an edge into it would assert a call that no
    /// execution ever makes.
    #[must_use]
    pub const fn callable(self) -> bool {
        !matches!(self, Self::Section)
    }
}

/// What a reference capture found.
///
/// The distinction is what lets `code_refs` name its counting unit honestly. Call references count
/// as caller/callee pairs; the rest are use sites with their own `file:line`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ReferenceKind {
    /// An invocation. The only kind that produces a call-graph edge.
    Call,
    /// An import, use, or require directive.
    Import,
    /// A supertype in an inheritance or conformance clause.
    Extends,
    /// A read of a name that is neither a call nor an import.
    Read,
}

impl ReferenceKind {
    /// The stable identifier reported in tool output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Import => "import",
            Self::Extends => "extends",
            Self::Read => "read",
        }
    }
}

/// The role a single query capture plays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureRole {
    /// The node whose span becomes a symbol.
    Definition(SymbolKind),
    /// The node whose span becomes a use site.
    Reference(ReferenceKind),
    /// The token carrying the name for whichever definition or reference it accompanies.
    Name,
    /// A documentation comment attached to the enclosing definition.
    Doc,
    /// The qualifier segment of a qualified reference, used by the precise resolution tiers.
    Qualifier,
    /// A capture the extractor ignores. Queries may name intermediate nodes for readability.
    Ignored,
}

impl CaptureRole {
    /// Classifies a capture name.
    ///
    /// Unknown capture names map to [`CaptureRole::Ignored`] rather than failing. A query is free to
    /// name a node purely to make a pattern readable, and refusing those would make every query
    /// author fight the table instead of writing the pattern they mean.
    #[must_use]
    pub fn classify(capture: &str) -> Self {
        if let Some(kind) = capture.strip_prefix("definition.") {
            return match kind {
                "function" => Self::Definition(SymbolKind::Function),
                "method" => Self::Definition(SymbolKind::Method),
                "class" | "struct" | "enum" | "type" => Self::Definition(SymbolKind::Class),
                "interface" | "trait" | "protocol" => Self::Definition(SymbolKind::Interface),
                "module" | "namespace" | "package" => Self::Definition(SymbolKind::Module),
                "macro" => Self::Definition(SymbolKind::Macro),
                "constant" | "const" => Self::Definition(SymbolKind::Constant),
                "field" | "property" => Self::Definition(SymbolKind::Field),
                "section" | "key" => Self::Definition(SymbolKind::Section),
                _ => Self::Ignored,
            };
        }
        if let Some(kind) = capture.strip_prefix("reference.") {
            return match kind {
                "call" => Self::Reference(ReferenceKind::Call),
                "import" | "include" | "require" => Self::Reference(ReferenceKind::Import),
                "extends" | "implementation" | "inherits" => {
                    Self::Reference(ReferenceKind::Extends)
                }
                "read" | "name" | "type" => Self::Reference(ReferenceKind::Read),
                _ => Self::Ignored,
            };
        }
        match capture {
            "name" => Self::Name,
            "doc" => Self::Doc,
            "qualifier" | "scope" => Self::Qualifier,
            _ => Self::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_outranks_function_on_specificity() {
        // Two captures can land on the same name token when a query has both a general and a
        // specific pattern. Dedup keeps the larger kind, so this ordering is what makes a method
        // record as a method rather than losing to whichever pattern matched second.
        assert!(SymbolKind::Method > SymbolKind::Function);
        assert!(SymbolKind::Class > SymbolKind::Function);
        assert!(SymbolKind::Function > SymbolKind::Section);
    }

    #[test]
    fn classifies_the_standard_capture_vocabulary() {
        assert_eq!(
            CaptureRole::classify("definition.function"),
            CaptureRole::Definition(SymbolKind::Function)
        );
        assert_eq!(
            CaptureRole::classify("definition.method"),
            CaptureRole::Definition(SymbolKind::Method)
        );
        assert_eq!(
            CaptureRole::classify("reference.call"),
            CaptureRole::Reference(ReferenceKind::Call)
        );
        assert_eq!(CaptureRole::classify("name"), CaptureRole::Name);
        assert_eq!(CaptureRole::classify("qualifier"), CaptureRole::Qualifier);
    }

    #[test]
    fn unknown_captures_are_ignored_rather_than_rejected() {
        assert_eq!(CaptureRole::classify("local.scope"), CaptureRole::Ignored);
        assert_eq!(
            CaptureRole::classify("definition.nonsense"),
            CaptureRole::Ignored
        );
    }

    #[test]
    fn sections_are_never_call_targets() {
        assert!(!SymbolKind::Section.callable());
        assert!(SymbolKind::Function.callable());
        assert!(SymbolKind::Method.callable());
    }
}
