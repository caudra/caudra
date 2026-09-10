use std::{ops::Range, path::Path};

use crate::{FilesystemError, path_policy::RootPathPolicy, types::FileDiff};

const TRUNCATION_MARKER: &str = "... (diff truncated)";
const CONTEXT_LINES: usize = 3;

/// One changed region as line spans into the old and new content. Supplied by a
/// caller that already knows where it changed the file, so the preview costs the
/// change rather than the distance between the first and last change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffHunk {
    pub(crate) old_start: usize,
    pub(crate) old_len: usize,
    pub(crate) new_start: usize,
    pub(crate) new_len: usize,
}

/// Render declared hunks as a multi-hunk preview under a hard byte bound.
/// Counts always describe the complete change, so dropping lines to fit the
/// bound never makes the reported totals disagree with what was applied.
pub(crate) fn file_diff_hunks(
    policy: &RootPathPolicy,
    file_path: &Path,
    old_content: &str,
    new_content: &str,
    hunks: &[DiffHunk],
    move_path: Option<&Path>,
    maximum_bytes: usize,
) -> Result<FileDiff, FilesystemError> {
    let old_name = policy.relative(file_path)?;
    let destination = move_path.unwrap_or(file_path);
    let new_name = policy.relative(destination)?;
    let old_lines = diff_lines(old_content);
    let new_lines = diff_lines(new_content);
    // A declared hunk can be coarser than the change it carries: a patch may
    // restate an unchanged line inside a chunk. Trimming each span to its
    // differing core keeps those lines as context instead of reporting them as
    // both removed and added.
    let trimmed = hunks
        .iter()
        .map(|hunk| trim_hunk(*hunk, &old_lines, &new_lines))
        .filter(|hunk| hunk.old_len > 0 || hunk.new_len > 0)
        .collect::<Vec<_>>();
    let hunks = trimmed.as_slice();
    let mut patch = BoundedPatch::new(maximum_bytes);
    patch.push_line("", &format!("--- {old_name}"));
    patch.push_line("", &format!("+++ {new_name}"));
    for group in group_hunks(hunks) {
        let first = hunks[group.start];
        let last = hunks[group.end - 1];
        let old_from = first.old_start.saturating_sub(CONTEXT_LINES);
        let lead = first.old_start.saturating_sub(old_from);
        let new_from = first.new_start.saturating_sub(lead);
        let old_change_end = first
            .old_start
            .max(last.old_start.saturating_add(last.old_len));
        let old_to = old_change_end
            .saturating_add(CONTEXT_LINES)
            .min(old_lines.len());
        let trail = old_to.saturating_sub(old_change_end);
        let new_to = last
            .new_start
            .saturating_add(last.new_len)
            .saturating_add(trail);
        patch.push_line(
            "",
            &format!(
                "@@ -{},{} +{},{} @@",
                old_from + 1,
                old_to.saturating_sub(old_from),
                new_from + 1,
                new_to.saturating_sub(new_from)
            ),
        );
        for line in window(&old_lines, old_from..first.old_start) {
            patch.push_line(" ", line);
        }
        for (position, hunk) in hunks[group.clone()].iter().enumerate() {
            for line in window(&old_lines, hunk.old_start..hunk.old_start + hunk.old_len) {
                patch.push_line("-", line);
            }
            for line in window(&new_lines, hunk.new_start..hunk.new_start + hunk.new_len) {
                patch.push_line("+", line);
            }
            let context_end = hunks[group.clone()]
                .get(position + 1)
                .map_or(old_to, |next| next.old_start);
            for line in window(&old_lines, hunk.old_start + hunk.old_len..context_end) {
                patch.push_line(" ", line);
            }
        }
    }
    Ok(FileDiff {
        file: destination.to_string_lossy().into_owned(),
        relative_path: new_name,
        patch: patch.text,
        additions: hunks.iter().map(|hunk| hunk.new_len).sum(),
        deletions: hunks.iter().map(|hunk| hunk.old_len).sum(),
        truncated: patch.truncated,
    })
}

fn trim_hunk(hunk: DiffHunk, old_lines: &[&str], new_lines: &[&str]) -> DiffHunk {
    let old = window(old_lines, hunk.old_start..hunk.old_start + hunk.old_len);
    let new = window(new_lines, hunk.new_start..hunk.new_start + hunk.new_len);
    let prefix = shared_prefix(old, new);
    let suffix = shared_suffix(old, new, prefix);
    DiffHunk {
        old_start: hunk.old_start + prefix,
        old_len: old.len() - prefix - suffix,
        new_start: hunk.new_start + prefix,
        new_len: new.len() - prefix - suffix,
    }
}

/// Merge hunks whose context windows touch so the preview never emits the same
/// context line twice or two adjacent headers for one readable region.
fn group_hunks(hunks: &[DiffHunk]) -> Vec<Range<usize>> {
    let mut groups: Vec<Range<usize>> = Vec::new();
    let mut window_end = 0usize;
    for (index, hunk) in hunks.iter().enumerate() {
        match groups.last_mut() {
            Some(group) if hunk.old_start.saturating_sub(CONTEXT_LINES) <= window_end => {
                group.end = index + 1;
            }
            _ => groups.push(index..index + 1),
        }
        window_end = hunk
            .old_start
            .saturating_add(hunk.old_len)
            .saturating_add(CONTEXT_LINES);
    }
    groups
}

fn window<'a>(lines: &'a [&'a str], range: Range<usize>) -> &'a [&'a str] {
    let start = range.start.min(lines.len());
    let end = range.end.min(lines.len()).max(start);
    &lines[start..end]
}

/// Produce the legacy compact single-hunk preview while enforcing a hard byte
/// bound during construction. Counts always describe the complete change.
pub(crate) fn file_diff(
    policy: &RootPathPolicy,
    file_path: &Path,
    old_content: &str,
    new_content: &str,
    move_path: Option<&Path>,
    maximum_bytes: usize,
) -> Result<FileDiff, FilesystemError> {
    let old_name = policy.relative(file_path)?;
    let destination = move_path.unwrap_or(file_path);
    let new_name = policy.relative(destination)?;
    let old_lines = diff_lines(old_content);
    let new_lines = diff_lines(new_content);
    let common_prefix = shared_prefix(&old_lines, &new_lines);
    let common_suffix = shared_suffix(&old_lines, &new_lines, common_prefix);
    let removed = &old_lines[common_prefix..old_lines.len() - common_suffix];
    let added = &new_lines[common_prefix..new_lines.len() - common_suffix];
    let context_start = common_prefix.saturating_sub(3);
    let before = &old_lines[context_start..common_prefix];
    let suffix_start = old_lines.len() - common_suffix;
    let after_end = (suffix_start + 3).min(old_lines.len());
    let after = &old_lines[suffix_start..after_end];
    let old_count = before.len() + removed.len() + after.len();
    let new_count = before.len() + added.len() + after.len();
    let mut patch = BoundedPatch::new(maximum_bytes);
    patch.push_line("", &format!("--- {old_name}"));
    patch.push_line("", &format!("+++ {new_name}"));
    patch.push_line(
        "",
        &format!(
            "@@ -{},{} +{},{} @@",
            context_start + 1,
            old_count,
            context_start + 1,
            new_count
        ),
    );
    for line in before {
        patch.push_line(" ", line);
    }
    for line in removed {
        patch.push_line("-", line);
    }
    for line in added {
        patch.push_line("+", line);
    }
    for line in after {
        patch.push_line(" ", line);
    }
    Ok(FileDiff {
        file: destination.to_string_lossy().into_owned(),
        relative_path: new_name,
        patch: patch.text,
        additions: added.len(),
        deletions: removed.len(),
        truncated: patch.truncated,
    })
}

/// Shorten an already-rendered preview to a byte allowance, reusing the marker
/// the renderer applies so a preview cut here is indistinguishable from one cut
/// during construction. Returns `None` when the preview already fits.
pub(crate) fn shorten_patch(patch: &str, maximum_bytes: usize) -> Option<String> {
    if patch.len() <= maximum_bytes {
        return None;
    }
    let mut bounded = BoundedPatch::new(maximum_bytes);
    bounded.text.push_str(patch);
    bounded.mark_truncated();
    Some(bounded.text)
}

struct BoundedPatch {
    text: String,
    maximum: usize,
    truncated: bool,
}

impl BoundedPatch {
    fn new(maximum: usize) -> Self {
        Self {
            text: String::with_capacity(maximum.min(4 * 1024)),
            maximum,
            truncated: false,
        }
    }

    fn push_line(&mut self, prefix: &str, line: &str) {
        if self.truncated {
            return;
        }
        let separator = usize::from(!self.text.is_empty());
        let Some(required) = self
            .text
            .len()
            .checked_add(separator)
            .and_then(|size| size.checked_add(prefix.len()))
            .and_then(|size| size.checked_add(line.len()))
        else {
            self.mark_truncated();
            return;
        };
        if required > self.maximum {
            self.mark_truncated();
            return;
        }
        if separator == 1 {
            self.text.push('\n');
        }
        self.text.push_str(prefix);
        self.text.push_str(line);
    }

    fn mark_truncated(&mut self) {
        self.truncated = true;
        let marker_bytes = TRUNCATION_MARKER.len().min(self.maximum);
        if self.maximum <= marker_bytes {
            self.text.clear();
            self.text.push_str(&TRUNCATION_MARKER[..marker_bytes]);
            return;
        }
        let reserve = marker_bytes + 1;
        let keep = self.maximum.saturating_sub(reserve);
        truncate_utf8(&mut self.text, keep);
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&TRUNCATION_MARKER[..marker_bytes]);
    }
}

fn truncate_utf8(value: &mut String, maximum: usize) {
    if value.len() <= maximum {
        return;
    }
    let mut boundary = maximum;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

fn diff_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines = content.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

fn shared_prefix(left: &[&str], right: &[&str]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn shared_suffix(left: &[&str], right: &[&str], prefix: usize) -> usize {
    let maximum = (left.len() - prefix).min(right.len() - prefix);
    (0..maximum)
        .take_while(|offset| left[left.len() - offset - 1] == right[right.len() - offset - 1])
        .count()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{DiffHunk, TRUNCATION_MARKER, file_diff, file_diff_hunks};
    use crate::path_policy::RootPathPolicy;

    async fn policy(root: &tempfile::TempDir) -> RootPathPolicy {
        RootPathPolicy::create(root.path()).await.expect("policy")
    }

    #[tokio::test]
    async fn a_preview_costs_the_change_not_the_distance_between_changes() {
        let root = tempdir().expect("root");
        let policy = policy(&root).await;
        let path = root.path().join("wide.txt");
        let old = (0..500)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        let new = old
            .replacen("line 0\n", "LINE 0\n", 1)
            .replace("line 499\n", "LINE 499\n");
        let hunks = [
            DiffHunk {
                old_start: 0,
                old_len: 1,
                new_start: 0,
                new_len: 1,
            },
            DiffHunk {
                old_start: 499,
                old_len: 1,
                new_start: 499,
                new_len: 1,
            },
        ];

        let diff = file_diff_hunks(&policy, &path, &old, &new, &hunks, None, 16 * 1024)
            .expect("multi-hunk diff");

        assert!(!diff.truncated);
        assert_eq!((diff.additions, diff.deletions), (2, 2));
        assert_eq!(diff.patch.matches("@@ -").count(), 2);
        // The single-span preview restates every line between the two changes.
        let span = file_diff(&policy, &path, &old, &new, None, 16 * 1024).expect("span diff");
        assert!(span.patch.len() > 10 * diff.patch.len());
    }

    #[tokio::test]
    async fn changes_within_one_context_window_merge_into_one_hunk() {
        let root = tempdir().expect("root");
        let policy = policy(&root).await;
        let path = root.path().join("near.txt");
        let old = (0..20)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        let new = old
            .replacen("line 5\n", "LINE 5\n", 1)
            .replacen("line 8\n", "LINE 8\n", 1);
        let hunks = [
            DiffHunk {
                old_start: 5,
                old_len: 1,
                new_start: 5,
                new_len: 1,
            },
            DiffHunk {
                old_start: 8,
                old_len: 1,
                new_start: 8,
                new_len: 1,
            },
        ];

        let diff = file_diff_hunks(&policy, &path, &old, &new, &hunks, None, 16 * 1024)
            .expect("merged diff");

        assert_eq!(diff.patch.matches("@@ -").count(), 1);
        // The shared context lines appear once, not once per hunk.
        assert_eq!(diff.patch.matches(" line 7").count(), 1);
        assert_eq!((diff.additions, diff.deletions), (2, 2));
    }

    #[tokio::test]
    async fn a_hunk_coarser_than_its_change_reports_only_what_differs() {
        let root = tempdir().expect("root");
        let policy = policy(&root).await;
        let path = root.path().join("coarse.txt");
        // A patch may restate an unchanged line inside a chunk. Reporting it as
        // both removed and added would double the preview and the counts.
        let hunks = [DiffHunk {
            old_start: 0,
            old_len: 2,
            new_start: 0,
            new_len: 2,
        }];

        let diff = file_diff_hunks(
            &policy,
            &path,
            "one\nthree\n",
            "ONE\nthree\n",
            &hunks,
            None,
            16 * 1024,
        )
        .expect("trimmed diff");

        assert_eq!((diff.additions, diff.deletions), (1, 1));
        assert!(diff.patch.ends_with("-one\n+ONE\n three"));
    }

    #[tokio::test]
    async fn bounds_construction_and_retains_complete_counts() {
        let root = tempdir().expect("root");
        let policy = RootPathPolicy::create(root.path()).await.expect("policy");
        let path = root.path().join("large.txt");
        let old = "removed\n".repeat(10_000);
        let diff =
            file_diff(&policy, &path, &old, "replacement\n", None, 128).expect("bounded diff");
        assert!(diff.truncated);
        assert!(diff.patch.len() <= 128);
        assert!(diff.patch.ends_with(TRUNCATION_MARKER));
        assert_eq!(diff.deletions, 10_000);
        assert_eq!(diff.additions, 1);
    }
}
