use std::time::Instant;

use tokio_util::sync::CancellationToken;
use tree_sitter::{Node, Tree};

use crate::FilesystemError;

use super::{IndexLimits, model::LineRange};

#[derive(Debug)]
pub(super) enum ParseFailure {
    Cancelled,
    Deadline,
    NodeLimit,
    DepthLimit,
    Parser,
}

impl ParseFailure {
    pub(super) fn into_filesystem_error(self) -> FilesystemError {
        match self {
            Self::Cancelled => FilesystemError::Aborted,
            Self::Deadline => FilesystemError::message("Index parser deadline exceeded"),
            Self::NodeLimit => FilesystemError::message("Index post-parse node limit exceeded"),
            Self::DepthLimit => FilesystemError::message("Index post-parse depth limit exceeded"),
            Self::Parser => FilesystemError::message("Index parser failed"),
        }
    }
}

pub(super) struct ExtractionGuard {
    cancellation: CancellationToken,
    deadline: Instant,
}

impl ExtractionGuard {
    pub(super) fn new(cancellation: CancellationToken, deadline: Instant) -> Self {
        Self {
            cancellation,
            deadline,
        }
    }

    pub(super) fn interrupted(&self) -> bool {
        self.cancellation.is_cancelled() || Instant::now() >= self.deadline
    }

    pub(super) fn check(&self) -> Result<(), ParseFailure> {
        if self.cancellation.is_cancelled() {
            Err(ParseFailure::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(ParseFailure::Deadline)
        } else {
            Ok(())
        }
    }

    pub(super) fn failure(&self) -> ParseFailure {
        if self.cancellation.is_cancelled() {
            ParseFailure::Cancelled
        } else if Instant::now() >= self.deadline {
            ParseFailure::Deadline
        } else {
            ParseFailure::Parser
        }
    }
}

pub(super) struct Context<'a> {
    source: &'a str,
    guard: &'a ExtractionGuard,
}

impl<'a> Context<'a> {
    pub(super) const fn new(source: &'a str, guard: &'a ExtractionGuard) -> Self {
        Self { source, guard }
    }

    pub(super) fn check(&self) -> Result<(), ParseFailure> {
        self.guard.check()
    }

    pub(super) fn text(&self, node: Node<'_>) -> &'a str {
        self.source
            .get(node.start_byte()..node.end_byte())
            .unwrap_or_default()
    }

    pub(super) fn children<'tree>(
        &self,
        node: Node<'tree>,
    ) -> Result<Vec<Node<'tree>>, ParseFailure> {
        self.check()?;
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .map(|child| {
                self.check()?;
                Ok(child)
            })
            .collect()
    }

    pub(super) fn named_children<'tree>(
        &self,
        node: Node<'tree>,
    ) -> Result<Vec<Node<'tree>>, ParseFailure> {
        self.check()?;
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .map(|child| {
                self.check()?;
                Ok(child)
            })
            .collect()
    }

    pub(super) fn fields<'tree>(
        &self,
        node: Node<'tree>,
        name: &str,
    ) -> Result<Vec<Node<'tree>>, ParseFailure> {
        self.check()?;
        let mut cursor = node.walk();
        node.children_by_field_name(name, &mut cursor)
            .map(|child| {
                self.check()?;
                Ok(child)
            })
            .collect()
    }

    pub(super) fn field<'tree>(
        &self,
        node: Node<'tree>,
        name: &str,
    ) -> Result<Option<Node<'tree>>, ParseFailure> {
        Ok(self.fields(node, name)?.into_iter().next())
    }

    pub(super) fn child<'tree>(
        &self,
        node: Node<'tree>,
        kind: &str,
    ) -> Result<Option<Node<'tree>>, ParseFailure> {
        Ok(self
            .children(node)?
            .into_iter()
            .find(|child| child.kind() == kind))
    }

    pub(super) fn range(&self, node: Node<'_>) -> LineRange {
        LineRange::from_node(node)
    }
}

pub(super) fn inspect_tree(
    tree: &Tree,
    limits: IndexLimits,
    guard: &ExtractionGuard,
) -> Result<(), ParseFailure> {
    let mut count = 0usize;
    let mut stack = vec![(tree.root_node(), 1usize)];
    while let Some((node, depth)) = stack.pop() {
        guard.check()?;
        count += 1;
        if count > limits.max_nodes {
            return Err(ParseFailure::NodeLimit);
        }
        if depth > limits.max_depth {
            return Err(ParseFailure::DepthLimit);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            guard.check()?;
            if depth + 1 > limits.max_depth {
                return Err(ParseFailure::DepthLimit);
            }
            if count.saturating_add(stack.len()) >= limits.max_nodes {
                return Err(ParseFailure::NodeLimit);
            }
            stack.push((child, depth + 1));
        }
    }
    Ok(())
}
