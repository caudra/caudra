//! Which lane each commit of the log belongs in.
//!
//! The sidebar is between 16 and 80 columns wide and the summary needs most of
//! them, so the graph draws two lanes rather than one per branch: the chain of
//! first parents leading back from `HEAD`, and everything a merge brought in.
//! A commit reachable only through a second parent hangs off the trunk no
//! matter how deep the branch it came from was.

use crate::scm::repo::Commit;

/// Where a commit sits, which is all the rail glyph has to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rail {
    /// On the chain of first parents from `HEAD`.
    Trunk,
    /// On that chain, and joining another line of development back into it.
    Merge,
    /// Reached only by following a merge's second parent.
    Side,
}

/// Classifies every commit of `log`, in the order it was walked.
///
/// The chain is followed by identifier rather than by position, because the
/// walk is ordered by commit time: a side branch's commits are interleaved with
/// the trunk's, so the parent of `log[n]` is not reliably `log[n + 1]`.
pub fn rails(log: &[Commit]) -> Vec<Rail> {
    let mut trunk = Vec::with_capacity(log.len());
    let mut next = log.first().map(|commit| commit.id.as_str());
    while let Some(id) = next {
        trunk.push(id);
        next = log
            .iter()
            .find(|commit| commit.id == id)
            .and_then(|commit| commit.parents.first())
            .map(String::as_str);
        // A history long enough to leave the window ends the chain, and so does
        // a cycle, which cannot happen in a commit graph but costs one
        // comparison to rule out.
        if next.is_some_and(|id| trunk.contains(&id)) {
            break;
        }
    }

    log.iter()
        .map(|commit| {
            match (
                trunk.contains(&commit.id.as_str()),
                commit.parents.len() > 1,
            ) {
                (true, true) => Rail::Merge,
                (true, false) => Rail::Trunk,
                (false, _) => Rail::Side,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Commit, Rail, rails};

    const WRONG_LANE: &str = "the commit was put in the wrong lane of the graph";

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id.to_owned(),
            summary: String::new(),
            author: String::new(),
            parents: parents.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    #[test]
    fn an_empty_log_has_no_rails() {
        assert!(rails(&[]).is_empty(), "{WRONG_LANE}");
    }

    #[test]
    fn a_linear_history_is_all_trunk() {
        let log = vec![commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        assert_eq!(rails(&log), vec![Rail::Trunk; 3], "{WRONG_LANE}");
    }

    #[test]
    fn a_merge_puts_its_second_parent_on_the_side() {
        // c merges the side commit s back into the trunk that runs c -> b -> a.
        let log = vec![
            commit("c", &["b", "s"]),
            commit("s", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ];
        assert_eq!(
            rails(&log),
            vec![Rail::Merge, Rail::Side, Rail::Trunk, Rail::Trunk],
            "{WRONG_LANE}"
        );
    }

    #[test]
    fn a_chain_that_leaves_the_window_stops_at_its_edge() {
        let log = vec![commit("c", &["b"]), commit("b", &["gone"])];
        assert_eq!(rails(&log), vec![Rail::Trunk; 2], "{WRONG_LANE}");
    }

    #[test]
    fn a_merge_off_the_trunk_is_still_a_side_commit() {
        let log = vec![
            commit("d", &["c", "s"]),
            commit("s", &["a", "t"]),
            commit("t", &["a"]),
            commit("c", &["a"]),
            commit("a", &[]),
        ];
        assert_eq!(
            rails(&log),
            vec![
                Rail::Merge,
                Rail::Side,
                Rail::Side,
                Rail::Trunk,
                Rail::Trunk
            ],
            "{WRONG_LANE}"
        );
    }
}
