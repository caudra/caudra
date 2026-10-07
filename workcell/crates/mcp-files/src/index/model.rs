use tree_sitter::Node;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Section {
    Import,
    Module,
    Constant,
    Rule,
    Instruction,
    Target,
    Block,
    Type,
    Trait,
    Impl,
    Function,
    Class,
    Macro,
    Heading,
}

impl Section {
    pub(super) const ALL: [Self; 14] = [
        Self::Import,
        Self::Module,
        Self::Constant,
        Self::Rule,
        Self::Instruction,
        Self::Target,
        Self::Block,
        Self::Type,
        Self::Trait,
        Self::Impl,
        Self::Function,
        Self::Class,
        Self::Macro,
        Self::Heading,
    ];

    pub(super) const fn header(self) -> &'static str {
        match self {
            Self::Import => "imports:",
            Self::Module => "mod:",
            Self::Constant => "consts:",
            Self::Rule => "rules:",
            Self::Instruction => "instructions:",
            Self::Target => "targets:",
            Self::Block => "blocks:",
            Self::Type => "types:",
            Self::Trait => "traits:",
            Self::Impl => "impls:",
            Self::Function => "fns:",
            Self::Class => "classes:",
            Self::Macro => "macros:",
            Self::Heading => "headings:",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LineRange {
    pub(super) start: usize,
    pub(super) end: usize,
}

impl LineRange {
    pub(super) fn from_node(node: Node<'_>) -> Self {
        Self {
            start: node.start_position().row + 1,
            end: node.end_position().row + 1,
        }
    }
}

#[derive(Debug)]
pub(super) struct Entry {
    pub(super) section: Section,
    pub(super) range: LineRange,
    pub(super) value: EntryValue,
}

impl Entry {
    pub(super) fn item(section: Section, node: Node<'_>, text: impl Into<String>) -> Self {
        Self {
            section,
            range: LineRange::from_node(node),
            value: EntryValue::Item(Item {
                text: text.into(),
                children: Vec::new(),
                attrs: Vec::new(),
                child_kind: ChildKind::Detailed,
            }),
        }
    }

    pub(super) fn import(node: Node<'_>, paths: Vec<Vec<String>>, keyword: Option<String>) -> Self {
        Self {
            section: Section::Import,
            range: LineRange::from_node(node),
            value: EntryValue::Import { paths, keyword },
        }
    }

    pub(super) fn item_mut(&mut self) -> &mut Item {
        let EntryValue::Item(item) = &mut self.value else {
            unreachable!("item-only mutation used for an import")
        };
        item
    }

    pub(super) fn text(&self) -> &str {
        match &self.value {
            EntryValue::Item(item) => &item.text,
            EntryValue::Import { .. } => "",
        }
    }
}

#[derive(Debug)]
pub(super) enum EntryValue {
    Item(Item),
    Import {
        paths: Vec<Vec<String>>,
        keyword: Option<String>,
    },
}

#[derive(Debug)]
pub(super) struct Item {
    pub(super) text: String,
    pub(super) children: Vec<Child>,
    pub(super) attrs: Vec<String>,
    pub(super) child_kind: ChildKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChildKind {
    Detailed,
    Brief,
}

#[derive(Debug)]
pub(super) enum Child {
    Text(String),
    Ranged { body: String, range: LineRange },
    Entry(Box<Entry>),
}

impl From<String> for Child {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Child {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<Entry> for Child {
    fn from(value: Entry) -> Self {
        Self::Entry(Box::new(value))
    }
}

#[derive(Default)]
pub(super) struct RawLineMetadata {
    pub(super) tag: Option<&'static str>,
    pub(super) body: Option<String>,
    pub(super) range: Option<String>,
}

pub(super) struct ParsedSkeleton {
    pub(super) skeleton: String,
    pub(super) metadata: Vec<RawLineMetadata>,
    pub(super) parse_error: bool,
}
