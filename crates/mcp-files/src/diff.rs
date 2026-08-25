use std::path::Path;

use crate::{FilesystemError, path_policy::RootPathPolicy, types::FileDiff};

const TRUNCATION_MARKER: &str = "... (diff truncated)";

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

    use super::{TRUNCATION_MARKER, file_diff};
    use crate::path_policy::RootPathPolicy;

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
