//! The note files as `/context` measures them, and the limits on what a
//! `memory` call writes and returns.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use caudra_providers::estimate_tokens;
use caudra_storage::local_documents::MemoryFileStat;

use crate::memory::store::{journal_body, local_listing};
use crate::memory::tree::VIEW;

/// Most bytes a write stores.
pub const MAX_FILE_BYTES: usize = 20 * 1024;
/// Most bytes of an answer the model is charged for: a whole view with its
/// heading and notices.
pub const MAX_OUTPUT_BYTES: usize = VIEW + OUTPUT_HEADROOM;
pub const CAP_HINT_REWRITE: &str = "rewrite the memory to fit under the cap";
pub const CAP_HINT_ZOOM: &str = "zoom into a line to read less at once";
pub const NO_MEMORIES: &str = "No memories yet.";

/// Room past the view's lines for a `view`'s heading and notices.
const OUTPUT_HEADROOM: usize = 1024;

pub struct Note {
    pub name: String,
    pub tokens: u32,
}

/// Every note in the directory with what reading it costs, by name.
/// Unreadable files are reported rather than dropped: a note the model wrote
/// and cannot read back is a problem worth surfacing.
pub fn scan(dir: &Path, cache: &mut TokenCache) -> (Vec<Note>, Vec<String>) {
    let Ok(Some(files)) = local_listing(dir) else {
        return (Vec::new(), Vec::new());
    };
    let (mut notes, mut warnings) = (Vec::new(), Vec::new());
    for stat in files {
        match cache.measure(dir, &stat) {
            Ok(tokens) => notes.push(Note {
                name: stat.name,
                tokens,
            }),
            Err(error) => warnings.push(format!("{}: {error}", stat.name)),
        }
    }
    (notes, warnings)
}

/// Keyed on size and mtime so an unchanged note is never re-read or
/// re-tokenized. Failures are never cached.
#[derive(Default)]
pub struct TokenCache {
    entries: HashMap<PathBuf, ((u64, i64), u32)>,
}

impl TokenCache {
    fn measure(&mut self, dir: &Path, stat: &MemoryFileStat) -> Result<u32, String> {
        let path = dir.join(&stat.name);
        let stamp = (stat.size, stat.modified_ms);
        if let Some(&(cached, tokens)) = self.entries.get(&path)
            && cached == stamp
        {
            return Ok(tokens);
        }
        let content = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let tokens = estimate_tokens(journal_body(&content));
        self.entries.insert(path, (stamp, tokens));
        Ok(tokens)
    }
}

/// Truncates on a codepoint boundary; a cut mid-sequence would hand the model
/// invalid UTF-8 to quote back.
pub fn cap(text: String, hint: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let cut = text.floor_char_boundary(MAX_OUTPUT_BYTES);
    format!(
        "{}\n... (output truncated at {MAX_OUTPUT_BYTES} bytes; {hint})",
        &text[..cut]
    )
}

pub fn write_size_error(content: &str) -> Option<String> {
    (content.len() > MAX_FILE_BYTES).then(|| {
        format!(
            "content exceeds {MAX_FILE_BYTES} bytes (got {}); split the memory or trim to stay under the cap",
            content.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::token_label;
    use test_case::test_case;

    const NOTE: &str = "a.md";
    const NESTED: &str = "sub/b.md";
    const BODY: &str = "remember this exact convention";
    const LONGER_BODY: &str = "a longer body that changes the count";
    const SIZE: u64 = 1;
    const STAMP: i64 = 0;
    const NEWER_STAMP: i64 = 1;

    fn write_note(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn stat(modified_ms: i64) -> MemoryFileStat {
        MemoryFileStat {
            name: NOTE.to_owned(),
            size: SIZE,
            modified_ms,
        }
    }

    #[test]
    fn a_scan_counts_each_note_without_its_frontmatter() {
        let temp = tempfile::tempdir().unwrap();
        write_note(temp.path(), NOTE, &format!("---\ntags: [a]\n---\n{BODY}"));
        write_note(temp.path(), NESTED, BODY);

        let (notes, warnings) = scan(temp.path(), &mut TokenCache::default());

        assert!(warnings.is_empty(), "{warnings:?}");
        let found: Vec<_> = notes
            .iter()
            .map(|note| (note.name.as_str(), note.tokens))
            .collect();
        assert_eq!(
            found,
            [
                (NOTE, estimate_tokens(BODY)),
                (NESTED, estimate_tokens(BODY))
            ]
        );
    }

    #[test]
    fn a_missing_directory_scans_as_empty() {
        let (notes, warnings) = scan(
            Path::new("/nonexistent/memories"),
            &mut TokenCache::default(),
        );
        assert!(notes.is_empty() && warnings.is_empty());
    }

    /// Re-reading every note on every scan would make `/context` cost scale
    /// with the project's history. Driven through the cache directly so the
    /// test controls the stamp instead of racing the filesystem clock.
    #[test]
    fn a_note_with_an_unchanged_stamp_is_not_reread() {
        let temp = tempfile::tempdir().unwrap();
        write_note(temp.path(), NOTE, BODY);
        let mut cache = TokenCache::default();
        assert_eq!(
            cache.measure(temp.path(), &stat(STAMP)).unwrap(),
            estimate_tokens(BODY)
        );

        write_note(temp.path(), NOTE, LONGER_BODY);
        assert_eq!(
            cache.measure(temp.path(), &stat(STAMP)).unwrap(),
            estimate_tokens(BODY),
            "same size and stamp must not trigger a re-read"
        );
        assert_eq!(
            cache.measure(temp.path(), &stat(NEWER_STAMP)).unwrap(),
            estimate_tokens(LONGER_BODY),
            "a new stamp must invalidate the count"
        );
    }

    #[test]
    fn an_unreadable_note_is_reported_and_not_cached() {
        let temp = tempfile::tempdir().unwrap();
        let mut cache = TokenCache::default();
        assert!(cache.measure(temp.path(), &stat(STAMP)).is_err());
        write_note(temp.path(), NOTE, BODY);
        assert_eq!(
            cache.measure(temp.path(), &stat(STAMP)).unwrap(),
            estimate_tokens(BODY)
        );
    }

    #[test]
    fn a_short_body_is_not_capped() {
        assert_eq!(cap("short".into(), CAP_HINT_REWRITE), "short");
    }

    #[test]
    fn an_oversized_body_is_capped_with_the_hint() {
        let capped = cap("x".repeat(MAX_OUTPUT_BYTES + 100), CAP_HINT_REWRITE);
        assert!(capped.contains(CAP_HINT_REWRITE), "{capped}");
        assert!(capped.starts_with(&"x".repeat(MAX_OUTPUT_BYTES)));
    }

    /// Cutting mid-codepoint would panic on the slice, and a byte-exact cap
    /// lands there for any multi-byte content.
    #[test]
    fn a_cap_never_splits_a_codepoint() {
        let capped = cap("é".repeat(MAX_OUTPUT_BYTES), CAP_HINT_REWRITE);
        assert!(capped.contains(CAP_HINT_REWRITE));
    }

    #[test]
    fn an_oversized_write_is_refused_before_it_reaches_disk() {
        assert!(write_size_error(&"x".repeat(MAX_FILE_BYTES + 1)).is_some());
        assert!(write_size_error(&"x".repeat(MAX_FILE_BYTES)).is_none());
    }

    #[test_case(0, "~0 tokens" ; "an_empty_note_reports_zero")]
    #[test_case(999, "~999 tokens" ; "counts_below_a_thousand_stay_exact")]
    #[test_case(1_000, "~1k tokens" ; "a_thousand_switches_to_the_k_suffix")]
    #[test_case(1_304, "~1.3k tokens" ; "larger_counts_round_to_one_decimal")]
    fn token_label_formats(tokens: u32, expected: &str) {
        assert_eq!(token_label(tokens), expected);
    }
}
