//! The summary tree over the memory journal, and the view folded from it.
//!
//! Entry `i` is the leaf `(0, i)`. Node `(l, i)` merges `(l-1, 2i)` and
//! `(l-1, 2i+1)`, covers the `2^l` entries from `i·2^l` on, and is addressed
//! `id+n`. The view is a list of nodes covering every entry, oldest first:
//! each entry appends its leaf, and while the lines are over budget the most
//! due pair of sibling lines whose parent is built merges into that parent.
//! So recent entries keep a line each and older lines cover more.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::{self, Write};
use std::ops::Range;

use thiserror::Error;

/// Most bytes a line of the tree holds.
pub const NODE: usize = 512;
/// Most bytes the view's lines hold.
pub const VIEW: usize = 32 * 1024;
pub const VIEW_DOC: &str = include_str!("../prompts/memory_view.md");
/// How the line of an entry not summarized yet begins.
pub const PENDING: &str = "(not summarized yet)";

const OPEN: &str = "\n<memory>\n";
const CLOSE: &str = "</memory>\n";
const CLOSE_LINE: &str = "\n</memory>\n";
const HIDDEN_PREFIX: &str = "(The ";
const HIDDEN_SUFFIX: &str =
    " oldest entries are left out until their summaries are written; `memory` search finds them.)";
const RULER: &str = "-";

/// A node of the tree: the `2^level` entries from `index·2^level` on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Part {
    pub level: u32,
    pub index: u64,
}

impl Part {
    pub fn leaf(seq: u64) -> Self {
        Self {
            level: 0,
            index: seq,
        }
    }

    /// The node addressed `id+n`.
    pub fn at(id: u64, n: u64) -> Result<Self, ZoomError> {
        if !n.is_power_of_two() {
            return Err(ZoomError::NotPowerOfTwo(n));
        }
        if !id.is_multiple_of(n) {
            return Err(ZoomError::Misaligned { id, n });
        }
        Ok(Self {
            level: n.trailing_zeros(),
            index: id / n,
        })
    }

    pub fn start(&self) -> u64 {
        self.index << self.level
    }

    pub fn count(&self) -> u64 {
        1 << self.level
    }

    pub fn end(&self) -> u64 {
        self.start() + self.count()
    }

    pub fn parent(&self) -> Self {
        Self {
            level: self.level + 1,
            index: self.index / 2,
        }
    }

    pub fn sibling(&self) -> Self {
        Self {
            level: self.level,
            index: self.index ^ 1,
        }
    }

    pub fn children(&self) -> Option<[Self; 2]> {
        let level = self.level.checked_sub(1)?;
        let index = self.index * 2;
        Some([
            Self { level, index },
            Self {
                level,
                index: index + 1,
            },
        ])
    }

    pub fn is_left(&self) -> bool {
        self.index.is_multiple_of(2)
    }

    /// Whether every entry the node covers exists yet.
    pub fn formed(&self, entries: u64) -> bool {
        self.end() <= entries
    }

    pub fn covers(&self, seq: u64) -> bool {
        (self.start()..self.end()).contains(&seq)
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{}", self.start(), self.count())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ZoomError {
    #[error("n must be a power of 2, not {0}")]
    NotPowerOfTwo(u64),
    #[error("id must be a multiple of n, and {id} is not a multiple of {n}")]
    Misaligned { id: u64, n: u64 },
    #[error("{id}+{n} runs past the last entry: the memory holds {entries} entries")]
    PastEnd { id: u64, n: u64, entries: u64 },
}

/// The node `zoom(id, n)` opens, in a memory of `entries` entries.
pub fn zoom(id: u64, n: u64, entries: u64) -> Result<Part, ZoomError> {
    let part = Part::at(id, n)?;
    if id.checked_add(n).is_none_or(|end| end > entries) {
        return Err(ZoomError::PastEnd { id, n, entries });
    }
    Ok(part)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafKind {
    Note,
    Delete,
    Forgotten,
}

impl LeafKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Delete => "delete",
            Self::Forgotten => "forgotten",
        }
    }
}

/// An entry of the journal as the tree reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    pub kind: LeafKind,
    pub name: String,
    pub heading: String,
    /// The note's text. A note without it is never kept verbatim, so callers
    /// may leave out bodies that [`fits`] says cannot be.
    pub body: Option<String>,
}

impl Leaf {
    /// The entry whole, when it is known.
    pub fn text(&self) -> Option<String> {
        match (self.kind, &self.body) {
            (LeafKind::Note, None) => None,
            (kind, body) => Some(entry_text(
                kind,
                &self.name,
                body.as_deref().unwrap_or_default(),
            )),
        }
    }

    fn placeholder(&self) -> String {
        let mut line = format!("{PENDING} {} {}", self.kind.as_str(), self.name);
        if !self.heading.is_empty() {
            let _ = write!(line, ": {}", self.heading);
        }
        line
    }
}

/// An entry as the compactor reads it, and as its own line when it fits.
pub fn entry_text(kind: LeafKind, name: &str, body: &str) -> String {
    match kind {
        LeafKind::Note => format!("{} {name}\n{body}", kind.as_str()),
        LeafKind::Delete | LeafKind::Forgotten => format!("{} {name}", kind.as_str()),
    }
}

/// Whether a note of `body_len` bytes is short enough to be its own line.
pub fn fits(name: &str, body_len: usize) -> bool {
    LeafKind::Note.as_str().len() + 1 + name.len() + 1 + body_len <= NODE
}

/// A node whose text exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub text: String,
    /// Kept word for word because it fits a line, with no model call.
    pub verbatim: bool,
}

/// A pair of sibling lines in the view, which merges into its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// Where the pair's first line sits in the view.
    pub at: usize,
    pub parent: Part,
    /// Whether the parent is built, so the fold may merge the pair.
    pub ready: bool,
}

#[derive(Debug, Default)]
pub struct Tree {
    leaves: Vec<Leaf>,
    built: HashMap<Part, Built>,
}

impl Tree {
    /// `stored` holds the nodes a model wrote. Nodes short enough to be kept
    /// verbatim are derived here instead, so every process agrees on them
    /// without a write.
    pub fn new(leaves: Vec<Leaf>, stored: impl IntoIterator<Item = (Part, String)>) -> Self {
        let entries = leaves.len() as u64;
        let mut built: HashMap<Part, Built> = stored
            .into_iter()
            .filter(|(part, _)| part.formed(entries))
            .map(|(part, text)| {
                let built = Built {
                    text,
                    verbatim: false,
                };
                (part, built)
            })
            .collect();
        for (seq, leaf) in (0..).zip(&leaves) {
            let part = Part::leaf(seq);
            if built.contains_key(&part) {
                continue;
            }
            if let Some(text) = leaf.text().filter(|text| text.len() <= NODE) {
                built.insert(
                    part,
                    Built {
                        text,
                        verbatim: true,
                    },
                );
            }
        }
        let mut level = 1;
        while let Some(count) = entries.checked_shr(level).filter(|&count| count > 0) {
            for index in 0..count {
                let part = Part { level, index };
                if built.contains_key(&part) {
                    continue;
                }
                let left = Part {
                    level: level - 1,
                    index: index * 2,
                };
                let joined = match (built.get(&left), built.get(&left.sibling())) {
                    (Some(a), Some(b)) if a.text.len() + 1 + b.text.len() <= NODE => {
                        format!("{}\n{}", a.text, b.text)
                    }
                    _ => continue,
                };
                built.insert(
                    part,
                    Built {
                        text: joined,
                        verbatim: true,
                    },
                );
            }
            level += 1;
        }
        Self { leaves, built }
    }

    pub fn entries(&self) -> u64 {
        self.leaves.len() as u64
    }

    pub fn leaf(&self, seq: u64) -> Option<&Leaf> {
        self.leaves.get(usize::try_from(seq).ok()?)
    }

    pub fn built(&self, part: &Part) -> Option<&Built> {
        self.built.get(part)
    }

    pub fn is_built(&self, part: &Part) -> bool {
        self.built.contains_key(part)
    }

    /// What a line shows: the node's text, else what is known before it.
    pub fn line(&self, part: &Part) -> Cow<'_, str> {
        if let Some(built) = self.built.get(part) {
            return Cow::Borrowed(&built.text);
        }
        match self.leaf(part.index).filter(|_| part.level == 0) {
            Some(leaf) => Cow::Owned(leaf.placeholder()),
            None => Cow::Borrowed(PENDING),
        }
    }

    /// The view at `budget` bytes of lines. A line, once merged, is never
    /// split again, and merges only wait on parents not built yet.
    pub fn fold(&self, budget: usize) -> Vec<Part> {
        let mut parts: Vec<Part> = Vec::new();
        let mut size = 0;
        for seq in 0..self.entries() {
            let leaf = Part::leaf(seq);
            size += self.line_size(&leaf);
            parts.push(leaf);
            while size > budget {
                let Some(at) = self.most_due(&parts, seq + 1) else {
                    break;
                };
                let parent = parts[at].parent();
                size = size - self.line_size(&parts[at]) - self.line_size(&parts[at + 1])
                    + self.line_size(&parent);
                parts[at] = parent;
                parts.remove(at + 1);
            }
        }
        parts
    }

    /// Every pair of sibling lines in `parts`, most due first: the order the
    /// fold merges them in once their parents are built.
    pub fn merge_queue(&self, parts: &[Part]) -> Vec<Merge> {
        let entries = self.entries();
        let mut queue: Vec<Merge> = pairs(parts)
            .map(|at| {
                let parent = parts[at].parent();
                Merge {
                    at,
                    ready: self.is_built(&parent),
                    parent,
                }
            })
            .collect();
        queue.sort_by(|a, b| compare_due(&parts[b.at], &parts[a.at], entries));
        queue
    }

    /// The nodes the compactor may start now, most urgent first: the first
    /// line not built yet, then merges whose halves are built, most due
    /// first. A node waits until every line before it is built, so its
    /// context never holds a placeholder.
    pub fn work(&self, parts: &[Part]) -> Vec<Part> {
        let pending = parts.iter().find(|part| !self.is_built(part));
        let limit = pending.map_or(self.entries(), Part::start);
        let mut crowns: Vec<Part> = Vec::new();
        for part in parts.iter().take_while(|part| part.end() <= limit) {
            let mut crown = part.clone();
            while self.is_built(&crown.parent()) {
                crown = crown.parent();
            }
            if crowns.last() != Some(&crown) {
                crowns.push(crown);
            }
        }
        let mut merges: Vec<&Part> = crowns
            .windows(2)
            .filter(|pair| pair[0].is_left() && pair[1] == pair[0].sibling())
            .map(|pair| &pair[0])
            .collect();
        merges.sort_by(|a, b| compare_due(b, a, self.entries()));
        pending
            .cloned()
            .into_iter()
            .chain(merges.into_iter().map(Part::parent))
            .collect()
    }

    /// The lines the compactor reads before building `node`: the view up to
    /// the node's first entry, stopping at the first line not built.
    pub fn context(&self, parts: &[Part], node: &Part) -> Vec<&str> {
        parts
            .iter()
            .take_while(|part| part.end() <= node.start())
            .map_while(|part| self.built(part).map(|built| built.text.as_str()))
            .collect()
    }

    /// The view's lines as the prompt carries them. While parents not built
    /// yet leave the view over budget, only the newest lines that fit are
    /// kept.
    pub fn block(&self, parts: &[Part], budget: usize) -> Block {
        let mut lines: Vec<Line> = parts
            .iter()
            .map(|part| Line::new(part.clone(), &self.line(part)))
            .collect();
        if lines.iter().map(Line::size).sum::<usize>() <= budget {
            return Block { hidden: 0, lines };
        }
        let room = budget.saturating_sub(hidden_line(self.entries()).len() + 1);
        let mut used = 0;
        let mut kept = 0;
        for line in lines.iter().rev() {
            used += line.size();
            if used > room {
                break;
            }
            kept += 1;
        }
        let lines = lines.split_off(lines.len() - kept);
        Block {
            hidden: lines
                .first()
                .map_or(self.entries(), |line| line.part.start()),
            lines,
        }
    }

    /// Bytes the lines of `parts` take in the view.
    pub fn size(&self, parts: &[Part]) -> usize {
        parts.iter().map(|part| self.line_size(part)).sum()
    }

    fn line_size(&self, part: &Part) -> usize {
        address_size(part) + 1 + self.line(part).len() + 1
    }

    /// The pair the fold merges next: the most due pair of sibling lines
    /// whose parent is built, the oldest of equals.
    fn most_due(&self, parts: &[Part], entries: u64) -> Option<usize> {
        pairs(parts)
            .filter(|&at| self.is_built(&parts[at].parent()))
            .min_by(|&a, &b| compare_due(&parts[b], &parts[a], entries))
    }
}

/// One line of the view: the node and its text, newlines as spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub part: Part,
    pub text: String,
}

impl Line {
    fn new(part: Part, text: &str) -> Self {
        Self {
            part,
            text: text.replace(['\n', '\r'], " "),
        }
    }

    /// Bytes the line takes in the view.
    pub fn size(&self) -> usize {
        address_size(&self.part) + 1 + self.text.len() + 1
    }

    pub fn pending(&self) -> bool {
        self.text.starts_with(PENDING)
    }
}

/// The view as the system prompt carries it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    /// How many of the oldest entries are left out to stay within budget.
    pub hidden: u64,
    pub lines: Vec<Line>,
}

impl Block {
    /// How many entries the block accounts for.
    pub fn through(&self) -> u64 {
        self.lines
            .last()
            .map_or(self.hidden, |line| line.part.end())
    }

    pub fn render(&self) -> String {
        let lines: usize = self.lines.iter().map(Line::size).sum();
        let mut out = String::with_capacity(VIEW_DOC.len() + OPEN.len() + lines + CLOSE.len());
        out.push_str(VIEW_DOC);
        out.push_str(OPEN);
        if self.hidden > 0 {
            out.push_str(&hidden_line(self.hidden));
            out.push('\n');
        }
        for line in &self.lines {
            let _ = writeln!(out, "{}|{}", line.part, line.text);
        }
        out.push_str(CLOSE);
        out
    }

    /// Reads a rendered block back out of a system prompt.
    pub fn parse(system: &str) -> Option<Self> {
        let rows = system[Self::range(system)?]
            .strip_prefix(VIEW_DOC)?
            .strip_prefix(OPEN)?
            .strip_suffix(CLOSE)?;
        let mut block = Self::default();
        for row in rows.lines() {
            if let Some(hidden) = row
                .strip_prefix(HIDDEN_PREFIX)
                .and_then(|rest| rest.strip_suffix(HIDDEN_SUFFIX))
            {
                block.hidden = hidden.parse().ok()?;
                continue;
            }
            let (address, text) = row.split_once('|')?;
            block.lines.push(Line {
                part: parse_address(address)?,
                text: text.to_owned(),
            });
        }
        Some(block)
    }

    /// Where a rendered block sits in a system prompt.
    pub fn range(system: &str) -> Option<Range<usize>> {
        let start = system.find(VIEW_DOC)?;
        let close = start + system[start..].find(CLOSE_LINE)?;
        Some(start..close + CLOSE_LINE.len())
    }
}

/// The nodes from the view line covering `seq` down to its entry: what the
/// agent zooms through to read the entry whole.
pub fn zoom_path(parts: &[Part], seq: u64) -> Vec<Part> {
    let mut path: Vec<Part> = parts
        .iter()
        .find(|part| part.covers(seq))
        .cloned()
        .into_iter()
        .collect();
    while let Some([left, right]) = path.last().and_then(Part::children) {
        path.push(if left.covers(seq) { left } else { right });
    }
    path
}

/// A ruler exactly [`NODE`] bytes long. Models cannot count bytes, so the
/// compactor shows them the limit instead.
pub fn scale() -> String {
    RULER.repeat(NODE)
}

/// Where each pair of sibling lines starts in `parts`.
fn pairs(parts: &[Part]) -> impl Iterator<Item = usize> + '_ {
    parts
        .windows(2)
        .enumerate()
        .filter(|(_, pair)| pair[0].is_left() && pair[1] == pair[0].sibling())
        .map(|(at, _)| at)
}

/// Orders pairs by how long ago they ended, measured in their own line size.
/// Measuring from a pair's first entry instead would merge old pairs near
/// ties, and keep rewriting old lines.
fn compare_due(a: &Part, b: &Part, entries: u64) -> Ordering {
    let ago = |left: &Part| u128::from((entries + 1).saturating_sub(left.parent().end()));
    (ago(a) << b.level).cmp(&(ago(b) << a.level))
}

fn address_size(part: &Part) -> usize {
    digits(part.start()) + 1 + digits(part.count())
}

fn digits(value: u64) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

fn hidden_line(hidden: u64) -> String {
    format!("{HIDDEN_PREFIX}{hidden}{HIDDEN_SUFFIX}")
}

fn parse_address(address: &str) -> Option<Part> {
    let (id, n) = address.split_once('+')?;
    let (id, n) = (id.parse::<u64>().ok()?, n.parse::<u64>().ok()?);
    id.checked_add(n)?;
    Part::at(id, n).ok()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const NAME: &str = "note.md";
    const HEADING: &str = "Heading";
    const SHORT: &str = "short";
    const SUMMARY: &str = "summary";

    fn leaf(kind: LeafKind, body: Option<&str>) -> Leaf {
        Leaf {
            kind,
            name: NAME.into(),
            heading: HEADING.into(),
            body: body.map(str::to_owned),
        }
    }

    fn long_note() -> Leaf {
        leaf(LeafKind::Note, Some(&"x".repeat(NODE)))
    }

    fn every_part(entries: u64) -> impl Iterator<Item = Part> {
        (0..u64::BITS).flat_map(move |level| {
            (0..entries.checked_shr(level).unwrap_or(0)).map(move |index| Part { level, index })
        })
    }

    /// A tree whose every node a model wrote, each `size` bytes long.
    fn summarized(entries: u64, size: usize) -> Tree {
        let leaves = (0..entries).map(|_| long_note()).collect();
        Tree::new(
            leaves,
            every_part(entries).map(|part| (part, "s".repeat(size))),
        )
    }

    fn parts(addresses: &[(u64, u64)]) -> Vec<Part> {
        addresses
            .iter()
            .map(|&(id, n)| Part::at(id, n).unwrap())
            .collect()
    }

    fn lines_size(tree: &Tree, parts: &[Part]) -> usize {
        parts.iter().map(|part| tree.line_size(part)).sum()
    }

    #[test_case(leaf(LeafKind::Note, Some(SHORT)), Some("note note.md\nshort") ; "short_note_is_its_own_line")]
    #[test_case(long_note(), None ; "long_note_waits_for_a_summary")]
    #[test_case(leaf(LeafKind::Note, None), None ; "note_without_body_waits_for_a_summary")]
    #[test_case(leaf(LeafKind::Delete, None), Some("delete note.md") ; "delete_is_its_own_line")]
    #[test_case(leaf(LeafKind::Forgotten, None), Some("forgotten note.md") ; "forgotten_is_its_own_line")]
    fn leaves_that_fit_are_kept_verbatim(entry: Leaf, expected: Option<&str>) {
        let tree = Tree::new(vec![entry], []);

        let built = tree.built(&Part::leaf(0));

        assert_eq!(built.map(|built| built.text.as_str()), expected);
        assert!(built.is_none_or(|built| built.verbatim));
    }

    #[test_case(SHORT, true ; "short_pair_is_joined")]
    #[test_case(&"x".repeat(NODE / 2), false ; "long_pair_waits_for_a_summary")]
    fn parents_that_fit_are_kept_verbatim(body: &str, joined: bool) {
        let note = leaf(LeafKind::Note, Some(body));
        let tree = Tree::new(vec![note.clone(), note], []);

        let built = tree.built(&Part::at(0, 2).unwrap());

        let text = format!("note {NAME}\n{body}");
        let expected = joined.then(|| format!("{text}\n{text}"));
        assert_eq!(built.map(|built| built.text.clone()), expected);
    }

    #[test]
    fn stored_summary_wins_over_verbatim() {
        let tree = Tree::new(
            vec![leaf(LeafKind::Note, Some(SHORT))],
            [(Part::leaf(0), SUMMARY.to_owned())],
        );

        let built = tree.built(&Part::leaf(0)).unwrap();

        assert_eq!(built.text, SUMMARY);
        assert!(!built.verbatim);
    }

    #[test_case(0, true ; "exactly_a_node_fits")]
    #[test_case(1, false ; "a_byte_more_does_not")]
    fn fits_agrees_with_the_entry_text(extra: usize, expected: bool) {
        let body_len = NODE - entry_text(LeafKind::Note, NAME, "").len() + extra;

        let text = entry_text(LeafKind::Note, NAME, &"x".repeat(body_len));

        assert_eq!(fits(NAME, body_len), expected);
        assert_eq!(text.len() <= NODE, expected);
    }

    #[test_case(HEADING, "(not summarized yet) note note.md: Heading" ; "with_heading")]
    #[test_case("", "(not summarized yet) note note.md" ; "without_heading")]
    fn unbuilt_leaf_shows_its_title(heading: &str, expected: &str) {
        let mut note = long_note();
        note.heading = heading.into();
        let tree = Tree::new(vec![note], []);

        assert_eq!(tree.line(&Part::leaf(0)), expected);
    }

    #[test]
    fn fold_stays_within_budget_once_parents_are_built() {
        let tree = summarized(548, NODE);

        let parts = tree.fold(VIEW);

        assert!(lines_size(&tree, &parts) <= VIEW);
        assert_eq!(parts.first().map(Part::start), Some(0));
        assert!(
            parts
                .windows(2)
                .all(|pair| pair[0].end() == pair[1].start())
        );
        assert_eq!(parts.last().map(Part::end), Some(548));
        assert!(parts.windows(2).all(|pair| pair[0].level >= pair[1].level));
    }

    #[test]
    fn fold_never_splits_a_line() {
        let budget = 4096;
        let mut previous: Vec<Part> = Vec::new();
        for entries in 1..200 {
            let parts = summarized(entries, 300).fold(budget);

            for old in &previous {
                assert!(
                    parts.iter().any(|new| new.level >= old.level
                        && new.covers(old.start())
                        && new.covers(old.end() - 1)),
                    "{old} was split at {entries} entries"
                );
            }
            previous = parts;
        }
    }

    #[test]
    fn fold_keeps_every_entry_under_budget() {
        let tree = Tree::new(vec![leaf(LeafKind::Note, Some(SHORT)); 4], []);

        assert_eq!(tree.fold(VIEW), parts(&[(0, 1), (1, 1), (2, 1), (3, 1)]));
    }

    #[test]
    fn pair_that_ended_longest_ago_in_its_own_size_merges_first() {
        let tree = summarized(10, 100);
        let view = parts(&[(0, 4), (4, 4), (8, 1), (9, 1)]);

        let queue = tree.merge_queue(&view);

        assert_eq!(
            queue.iter().map(|merge| merge.at).collect::<Vec<_>>(),
            [2, 0]
        );
        assert_eq!(tree.most_due(&view, 10), Some(2));
    }

    #[test]
    fn fold_merges_the_first_ready_pair_of_the_queue() {
        let unbuilt = Part::at(8, 2).unwrap();
        let leaves = (0..10).map(|_| long_note()).collect();
        let stored = every_part(10)
            .filter(|part| *part != unbuilt)
            .map(|part| (part, "s".repeat(300)));
        let tree = Tree::new(leaves, stored);
        let view = parts(&[(0, 4), (4, 4), (8, 1), (9, 1)]);

        let queue = tree.merge_queue(&view);

        assert_eq!(queue[0].parent, unbuilt);
        assert!(!queue[0].ready);
        assert_eq!(
            tree.most_due(&view, 10),
            queue.iter().find(|merge| merge.ready).map(|merge| merge.at)
        );
    }

    #[test]
    fn view_over_budget_keeps_the_newest_lines() {
        let tree = Tree::new(vec![long_note(); 20], []);
        let budget = 400;

        let block = tree.block(&tree.fold(budget), budget);

        let kept: usize = block.lines.iter().map(Line::size).sum();
        assert!(kept + hidden_line(block.hidden).len() < budget);
        assert!(!block.lines.is_empty());
        assert_eq!(block.hidden, block.lines[0].part.start());
        assert_eq!(block.through(), 20);
        assert!(block.lines.iter().all(Line::pending));
        assert_eq!(Block::parse(&block.render()), Some(block));
    }

    #[test]
    fn view_within_budget_keeps_every_line() {
        let tree = summarized(8, 10);

        let block = tree.block(&tree.fold(VIEW), VIEW);

        assert_eq!(block.hidden, 0);
        assert_eq!(block.lines.len(), 8);
    }

    #[test_case(8, 2, 10, Ok(Part { level: 1, index: 4 }) ; "aligned_power_of_two")]
    #[test_case(9, 1, 10, Ok(Part::leaf(9)) ; "last_entry")]
    #[test_case(0, 3, 10, Err(ZoomError::NotPowerOfTwo(3)) ; "not_a_power_of_two")]
    #[test_case(0, 0, 10, Err(ZoomError::NotPowerOfTwo(0)) ; "zero_count")]
    #[test_case(2, 4, 10, Err(ZoomError::Misaligned { id: 2, n: 4 }) ; "misaligned")]
    #[test_case(8, 4, 10, Err(ZoomError::PastEnd { id: 8, n: 4, entries: 10 }) ; "past_the_end")]
    #[test_case(u64::MAX - 1, 2, 10, Err(ZoomError::PastEnd { id: u64::MAX - 1, n: 2, entries: 10 }) ; "overflowing")]
    fn zoom_validates_its_address(
        id: u64,
        n: u64,
        entries: u64,
        expected: Result<Part, ZoomError>,
    ) {
        assert_eq!(zoom(id, n, entries), expected);
    }

    #[test]
    fn scale_is_exactly_a_node() {
        assert_eq!(scale().len(), NODE);
    }

    #[test_case(0 ; "every_entry_shown")]
    #[test_case(4 ; "oldest_left_out")]
    fn block_round_trips_through_a_system_prompt(hidden: u64) {
        let block = Block {
            hidden,
            lines: vec![
                Line::new(Part::at(hidden, 4).unwrap(), "two\nlines </memory>"),
                Line::new(Part::leaf(hidden + 4), &long_note().placeholder()),
                Line::new(
                    Part::leaf(hidden + 5),
                    &entry_text(LeafKind::Delete, NAME, ""),
                ),
            ],
        };
        let rendered = block.render();
        let system = format!("instructions\n\n{rendered}\nreminders");

        assert_eq!(Block::parse(&system), Some(block));
        assert_eq!(
            Block::range(&system).map(|range| &system[range]),
            Some(rendered.as_str())
        );
    }

    #[test]
    fn prompt_without_block_parses_to_none() {
        assert_eq!(Block::parse("instructions"), None);
    }

    #[test]
    fn zoom_path_runs_from_the_view_line_to_the_entry() {
        let tree = summarized(64, 200);
        let view = tree.fold(2048);
        let seq = 21;

        let path = zoom_path(&view, seq);

        assert!(view.contains(&path[0]));
        assert_eq!(path.len() as u32, path[0].level + 1);
        assert_eq!(path.last(), Some(&Part::leaf(seq)));
        assert!(path.windows(2).all(|pair| {
            pair[0]
                .children()
                .is_some_and(|children| children.contains(&pair[1]))
        }));
    }

    #[test]
    fn work_starts_with_the_first_pending_line_then_the_most_due_merges() {
        let mut leaves = vec![long_note(); 6];
        leaves[5] = leaf(LeafKind::Note, Some(SHORT));
        let stored = (0..4).map(|seq| (Part::leaf(seq), "s".repeat(300)));
        let tree = Tree::new(leaves, stored);
        let view = tree.fold(VIEW);

        assert_eq!(tree.work(&view), parts(&[(4, 1), (0, 2), (2, 2)]));
    }

    #[test]
    fn work_merges_built_halves_above_the_view() {
        let leaves = vec![long_note(); 4];
        let stored = (0..4)
            .map(Part::leaf)
            .chain(parts(&[(0, 2), (2, 2)]))
            .map(|part| (part, "s".repeat(300)));
        let tree = Tree::new(leaves, stored);
        let view = tree.fold(VIEW);

        assert_eq!(view.len(), 4);
        assert_eq!(tree.work(&view), parts(&[(0, 4)]));
    }

    #[test]
    fn context_ends_before_the_node_and_at_the_first_pending_line() {
        let leaves = vec![long_note(); 4];
        let stored = [0, 1, 3].map(|seq| (Part::leaf(seq), format!("line {seq}")));
        let tree = Tree::new(leaves, stored);
        let view = tree.fold(VIEW);

        assert_eq!(tree.context(&view, &Part::leaf(1)), ["line 0"]);
        assert_eq!(tree.context(&view, &Part::leaf(3)), ["line 0", "line 1"]);
    }
}
