//! Maps a screen cell back to the component that drew it.
//!
//! Ratatui is immediate mode, so there is no retained tree to hit test against.
//! This crate records one while the frame is drawn: [`grab_scope!`] opens a
//! [`Scope`] that notes the component's name, its `file:line`, and the [`Rect`]
//! it was handed, then closes it on drop. Push order is z-order and nesting is
//! tracked as a depth, which is enough for [`stack_at`] to rebuild the chain of
//! components covering a point, innermost first.
//!
//! Recording lives in a thread local rather than a context object because
//! component render methods take `(&mut self, frame, area)` and cannot reach the
//! application state; threading a recorder through would mean changing every
//! signature.
//!
//! Everything is gated on `debug_assertions`. The macro expands to nothing in
//! release, so call sites vanish instead of calling a no-op, and the storage is
//! not compiled at all.
//!
//! Widgets rendered off the main thread, such as message bodies prepared by a
//! render worker and blitted back as a buffer snapshot, record into that
//! thread's storage and are dropped. Their content is attributed to the
//! component that blits it.

use ratatui::layout::{Position, Rect};

#[cfg(debug_assertions)]
use std::cell::{Cell, RefCell};
#[cfg(debug_assertions)]
use std::panic::Location;

/// Nodes kept per frame. A full screen records on the order of a hundred; the
/// cap only stops a runaway loop from growing the buffer without bound.
#[cfg(debug_assertions)]
const NODE_CAP: usize = 512;

/// One component's contribution to a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub name: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub area: Rect,
    pub depth: u16,
}

/// Open while a component draws. Dropping it closes the component's subtree.
#[must_use = "a scope closes as soon as it is dropped, so binding it is what gives it a body"]
pub struct Scope(());

impl Drop for Scope {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[cfg(debug_assertions)]
thread_local! {
    static LIVE: RefCell<Vec<Node>> = const { RefCell::new(Vec::new()) };
    static LAST: RefCell<Vec<Node>> = const { RefCell::new(Vec::new()) };
    static DEPTH: Cell<u16> = const { Cell::new(0) };
}

/// Opens a scope for the component drawing into `area`, attributed to the call
/// site. Prefer [`grab_scope!`], which costs nothing in release.
#[track_caller]
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
pub fn scope(name: &'static str, area: Rect) -> Scope {
    #[cfg(debug_assertions)]
    {
        let caller = Location::caller();
        let depth = DEPTH.with(|depth| {
            let current = depth.get();
            depth.set(current.saturating_add(1));
            current
        });
        LIVE.with_borrow_mut(|nodes| {
            if nodes.len() < NODE_CAP {
                nodes.push(Node {
                    name,
                    file: caller.file(),
                    line: caller.line(),
                    area,
                    depth,
                });
            }
        });
    }
    Scope(())
}

/// Records a component with no children of its own, such as one row of a list
/// the enclosing scope paints. A scope cannot serve here: a row's rect is known
/// only once it has been placed, which is after the point a scope would have to
/// open. Prefer [`grab_leaf!`], which costs nothing in release.
#[track_caller]
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
pub fn leaf(name: &'static str, area: Rect) {
    #[cfg(debug_assertions)]
    {
        let caller = Location::caller();
        let depth = DEPTH.with(Cell::get);
        LIVE.with_borrow_mut(|nodes| {
            if nodes.len() < NODE_CAP {
                nodes.push(Node {
                    name,
                    file: caller.file(),
                    line: caller.line(),
                    area,
                    depth,
                });
            }
        });
    }
}

/// Starts recording a frame, discarding whatever the previous one left behind.
pub fn begin_frame() {
    #[cfg(debug_assertions)]
    {
        LIVE.with_borrow_mut(Vec::clear);
        DEPTH.with(|depth| depth.set(0));
    }
}

/// Publishes the frame just drawn as the one [`stack_at`] answers from. Anything
/// drawn after this call is invisible to hit testing, which is how the grab
/// interface stays out of its own results.
pub fn end_frame() {
    #[cfg(debug_assertions)]
    LAST.with_borrow_mut(|last| LIVE.with_borrow_mut(|live| std::mem::swap(last, live)));
}

/// The components covering `pos` in the last published frame, innermost first.
/// Empty when nothing covers it, and always empty in release builds.
#[cfg(debug_assertions)]
pub fn stack_at(pos: Position) -> Vec<Node> {
    LAST.with_borrow(|nodes| stack_in(nodes, pos))
}

#[cfg(not(debug_assertions))]
pub fn stack_at(_pos: Position) -> Vec<Node> {
    Vec::new()
}

/// Walks the frame backwards, so the topmost component covering `pos` is the
/// leaf, then keeps only strictly shallower nodes that also cover it. A node
/// deeper than the leaf was drawn earlier and is therefore painted over.
#[cfg(debug_assertions)]
fn stack_in(nodes: &[Node], pos: Position) -> Vec<Node> {
    let mut stack: Vec<Node> = Vec::new();
    for node in nodes.iter().rev() {
        if !node.area.contains(pos) {
            continue;
        }
        if stack.last().is_some_and(|leaf| node.depth >= leaf.depth) {
            continue;
        }
        let root = node.depth == 0;
        stack.push(*node);
        if root {
            break;
        }
    }
    stack
}

/// Records the component drawing into `area` for the rest of the enclosing
/// block. Expands to nothing unless `debug_assertions` is on.
#[macro_export]
macro_rules! grab_scope {
    ($name:expr, $area:expr) => {
        #[cfg(debug_assertions)]
        let _grab_scope = $crate::scope($name, $area);
    };
}

/// Records a childless component occupying `area`. Expands to nothing unless
/// `debug_assertions` is on, so neither the call nor its arguments reach a
/// release build.
///
/// A leaf's rect is often computed at the call site purely to pass here. Gate
/// that with `#[cfg(debug_assertions)]` as well rather than leaving it to the
/// optimizer: a binding used only by this macro is unused in release, and
/// dead-code elimination is not a guarantee the language makes.
#[macro_export]
macro_rules! grab_leaf {
    ($name:expr, $area:expr) => {
        #[cfg(debug_assertions)]
        $crate::leaf($name, $area);
    };
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use test_case::test_case;

    const ROOT: &str = "root";
    const BRANCH: &str = "branch";
    const LEAF: &str = "leaf";
    const OVERLAY: &str = "overlay";
    const SIBLING: &str = "sibling";

    fn node(name: &'static str, depth: u16, area: Rect) -> Node {
        Node {
            name,
            file: file!(),
            line: line!(),
            area,
            depth,
        }
    }

    fn names(stack: &[Node]) -> Vec<&'static str> {
        stack.iter().map(|node| node.name).collect()
    }

    /// root covers the screen, branch its left half, leaf a box inside branch.
    fn nest() -> Vec<Node> {
        vec![
            node(ROOT, 0, Rect::new(0, 0, 40, 20)),
            node(BRANCH, 1, Rect::new(0, 0, 20, 20)),
            node(LEAF, 2, Rect::new(2, 2, 8, 4)),
        ]
    }

    #[test]
    fn nested_scopes_resolve_innermost_first() {
        let stack = stack_in(&nest(), Position::new(3, 3));
        assert_eq!(names(&stack), vec![LEAF, BRANCH, ROOT]);
    }

    #[test_case(Position::new(15, 10), vec![BRANCH, ROOT] ; "inside_branch_outside_leaf")]
    #[test_case(Position::new(30, 10), vec![ROOT] ; "outside_branch")]
    #[test_case(Position::new(60, 10), Vec::new() ; "outside_every_node")]
    fn partial_coverage_yields_the_covering_chain(pos: Position, expected: Vec<&'static str>) {
        assert_eq!(names(&stack_in(&nest(), pos)), expected);
    }

    #[test]
    fn later_sibling_wins_the_overlap() {
        let mut nodes = nest();
        nodes.push(node(SIBLING, 1, Rect::new(0, 0, 40, 20)));
        let stack = stack_in(&nodes, Position::new(3, 3));
        assert_eq!(names(&stack), vec![SIBLING, ROOT]);
    }

    /// An overlay drawn last is on top even though the node it covers was
    /// deeper, so the deeper node must not surface as the leaf.
    #[test]
    fn shallow_overlay_masks_deeper_nodes_drawn_earlier() {
        let mut nodes = nest();
        nodes.push(node(OVERLAY, 1, Rect::new(2, 2, 4, 2)));
        let stack = stack_in(&nodes, Position::new(3, 3));
        assert_eq!(names(&stack), vec![OVERLAY, ROOT]);
    }

    #[test]
    fn a_frame_without_a_root_still_reports_what_it_has() {
        let nodes = vec![node(LEAF, 3, Rect::new(0, 0, 10, 10))];
        assert_eq!(names(&stack_in(&nodes, Position::new(1, 1))), vec![LEAF]);
    }

    #[test]
    fn scopes_record_depth_and_publish_on_end_frame() {
        begin_frame();
        {
            let _outer = scope(ROOT, Rect::new(0, 0, 10, 10));
            let _inner = scope(LEAF, Rect::new(1, 1, 2, 2));
        }
        let reused = scope(SIBLING, Rect::new(5, 5, 2, 2));
        drop(reused);
        end_frame();

        let depths = LAST.with_borrow(|nodes| {
            nodes
                .iter()
                .map(|node| (node.name, node.depth))
                .collect::<Vec<_>>()
        });
        assert_eq!(depths, vec![(ROOT, 0), (LEAF, 1), (SIBLING, 0)]);
        assert_eq!(names(&stack_at(Position::new(1, 1))), vec![LEAF, ROOT]);
    }

    /// A leaf sits at the depth a scope opened there would have taken, so the
    /// enclosing scope resolves as its parent rather than its peer.
    #[test]
    fn a_leaf_nests_under_the_scope_that_recorded_it() {
        begin_frame();
        {
            let _outer = scope(ROOT, Rect::new(0, 0, 10, 10));
            leaf(LEAF, Rect::new(1, 1, 2, 2));
            leaf(SIBLING, Rect::new(5, 5, 2, 2));
        }
        end_frame();
        assert_eq!(names(&stack_at(Position::new(1, 1))), vec![LEAF, ROOT]);
        assert_eq!(names(&stack_at(Position::new(5, 5))), vec![SIBLING, ROOT]);
        assert_eq!(names(&stack_at(Position::new(9, 9))), vec![ROOT]);
    }

    #[test]
    fn begin_frame_discards_an_abandoned_frame() {
        begin_frame();
        drop(scope(ROOT, Rect::new(0, 0, 10, 10)));
        begin_frame();
        drop(scope(BRANCH, Rect::new(0, 0, 10, 10)));
        end_frame();
        assert_eq!(names(&stack_at(Position::new(1, 1))), vec![BRANCH]);
    }
}
