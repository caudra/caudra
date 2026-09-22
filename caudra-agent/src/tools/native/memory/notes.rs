//! Reading, tagging, and formatting the note files themselves.
//!
//! Tags are the index: a note is found by tag, never by scanning bodies. They
//! come from YAML frontmatter, falling back to the filename stem so an
//! untagged file is still reachable.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use caudra_providers::estimate_tokens;

pub const MAX_TAGS: usize = 50;
pub const MAX_FILE_BYTES: usize = 20 * 1024;
pub const CAP_HINT_REWRITE: &str = "rewrite the memory to fit under the cap";
pub const CAP_HINT_NARROW: &str = "narrow tags or read files by path";
pub const CAP_HINT_FILTER: &str = "pass tags=[...] to filter the list";
pub const NO_MEMORIES: &str = "No memories yet.";
pub const NO_MATCH: &str =
    "no memory files matched any of the given tags; use `list` to see available tags";

const MAX_TAG_LEN: usize = 64;
const MAX_REJECT_DISPLAY: usize = 64;
const WRITE_REJECT_PREFIX: &str = "invalid tag(s) rejected: ";
const READ_REJECT_PREFIX: &str = "warning: ignored invalid tag(s): ";
const UNREADABLE_PREFIX: &str = "warning: unreadable memory files: ";
const UNTAGGED: &str = "untagged";
const FRONTMATTER_FENCE: &str = "---";

pub struct Note {
    pub name: String,
    pub tokens: u32,
    pub tags: Vec<String>,
}

/// A tag and the notes carrying it. Ordered most-used first, so the prompt's
/// tag line leads with the ones worth reusing.
pub struct TagGroup {
    pub tag: String,
    pub files: Vec<(String, u32)>,
}

/// Coerces rather than rejects: any run of non-alphanumerics becomes `_`.
/// Returns `None` only when nothing survives.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut pending_underscore = false;
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            if pending_underscore && !out.is_empty() {
                out.push('_');
            }
            pending_underscore = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_underscore = true;
        }
    }
    if out.is_empty() {
        return None;
    }
    out.truncate(
        out.char_indices()
            .nth(MAX_TAG_LEN)
            .map_or(out.len(), |(i, _)| i),
    );
    Some(out)
}

/// Returns `(normalized, rejected, coerced)`. `coerced` records the tags that
/// changed spelling so a write can tell the model what it actually stored.
pub fn normalize_tags(raw: &[String]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut seen = HashSet::new();
    let (mut out, mut rejected, mut coerced) = (Vec::new(), Vec::new(), Vec::new());
    for tag in raw {
        match normalize_tag(tag) {
            Some(normalized) => {
                if normalized != *tag {
                    coerced.push(format!("{tag} -> {normalized}"));
                }
                if seen.insert(normalized.clone()) {
                    out.push(normalized);
                }
            }
            None => rejected.push(tag.clone()),
        }
    }
    (out, rejected, coerced)
}

fn format_rejected(rejected: &[String]) -> Option<String> {
    if rejected.is_empty() {
        return None;
    }
    Some(
        rejected
            .iter()
            .map(|tag| match tag.char_indices().nth(MAX_REJECT_DISPLAY) {
                Some((cut, _)) => format!("{}...", &tag[..cut]),
                None => tag.clone(),
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Tags a write will store. Unlike a read, a bad tag is fatal: silently
/// dropping it would file the note somewhere the model did not ask for.
pub fn tags_for_write(raw: &[String]) -> Result<(Vec<String>, Option<String>), String> {
    let (normalized, rejected, coerced) = normalize_tags(raw);
    if let Some(rejected) = format_rejected(&rejected) {
        return Err(format!("{WRITE_REJECT_PREFIX}{rejected}"));
    }
    let note = (!coerced.is_empty()).then(|| format!("normalized: {}", coerced.join(", ")));
    Ok((normalized, note))
}

/// Tags to filter by. Returns `(wanted, warning)`; a partial rejection is only
/// a warning because the surviving tags still describe a useful query.
pub fn tags_for_filter(raw: &[String]) -> Result<(HashSet<String>, Option<String>), String> {
    let (normalized, rejected, _) = normalize_tags(raw);
    let rejected = format_rejected(&rejected);
    if normalized.is_empty() {
        let detail = rejected.map_or(String::new(), |r| format!("; rejected: {r}"));
        return Err(format!("no valid tags after normalization{detail}"));
    }
    Ok((
        normalized.into_iter().collect(),
        rejected.map(|r| format!("{READ_REJECT_PREFIX}{r}")),
    ))
}

fn stem_tag(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    normalize_tag(stem).unwrap_or_else(|| UNTAGGED.to_owned())
}

/// Splits leading YAML frontmatter from the body. An unterminated fence is not
/// frontmatter: treating it as such would swallow the whole note.
pub fn parse_frontmatter(content: &str) -> (Option<serde_yaml::Value>, &str) {
    let rest = match content.trim_start_matches([' ', '\t', '\n', '\r']) {
        r if r.starts_with(FRONTMATTER_FENCE) => &r[FRONTMATTER_FENCE.len()..],
        _ => return (None, content),
    };
    let Some(rest) = rest.strip_prefix('\n') else {
        return (None, content);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, content);
    };
    let body = rest[end + "\n---".len()..].trim_start_matches(['\n', '\r']);
    (serde_yaml::from_str(&rest[..end]).ok(), body.trim_end())
}

pub fn tags_from_frontmatter(frontmatter: Option<&serde_yaml::Value>) -> Option<Vec<String>> {
    let raw = frontmatter?.get("tags")?;
    let listed = match raw {
        serde_yaml::Value::String(one) => vec![one.clone()],
        serde_yaml::Value::Sequence(many) => many
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => return None,
    };
    let (normalized, ..) = normalize_tags(&listed);
    (!normalized.is_empty()).then_some(normalized)
}

/// Every note in the directory, sorted by name. Unreadable files are reported
/// rather than dropped: a note the model wrote and cannot read back is a
/// problem worth surfacing.
pub fn scan(dir: &Path, cache: &mut TagCache) -> (Vec<Note>, Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    // `fs::metadata` rather than `DirEntry::metadata`: a symlinked note is a
    // note, and the latter would report the link instead.
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = fs::metadata(entry.path()).ok()?;
            let name = entry.file_name().to_str()?.to_owned();
            meta.is_file()
                .then(|| (name, meta.len(), meta.modified().ok()))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let (mut notes, mut warnings) = (Vec::new(), Vec::new());
    for (name, size, mtime) in files {
        match cache.measure(dir, &name, size, mtime) {
            Ok((tags, tokens)) => notes.push(Note { name, tokens, tags }),
            Err(error) => warnings.push(format!("{name}: {error}")),
        }
    }
    (notes, warnings)
}

/// Keyed on size and mtime so an unchanged file is never re-read, which also
/// keeps it from being re-tokenized. Failures are never cached: a transient
/// error must not pin a stem tag forever.
#[derive(Default)]
pub struct TagCache {
    entries: BTreeMap<PathBuf, CacheEntry>,
}

struct CacheEntry {
    size: u64,
    mtime: SystemTime,
    tags: Vec<String>,
    tokens: u32,
}

impl TagCache {
    fn measure(
        &mut self,
        dir: &Path,
        name: &str,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> Result<(Vec<String>, u32), String> {
        let path = dir.join(name);
        if let Some(mtime) = mtime
            && let Some(cached) = self.entries.get(&path)
            && cached.size == size
            && cached.mtime == mtime
        {
            return Ok((cached.tags.clone(), cached.tokens));
        }
        let content = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let (frontmatter, body) = parse_frontmatter(&content);
        let tags =
            tags_from_frontmatter(frontmatter.as_ref()).unwrap_or_else(|| vec![stem_tag(name)]);
        let tokens = estimate_tokens(body);
        if let Some(mtime) = mtime {
            self.entries.insert(
                path,
                CacheEntry {
                    size,
                    mtime,
                    tags: tags.clone(),
                    tokens,
                },
            );
        }
        Ok((tags, tokens))
    }
}

/// Most-used tag first so the prompt's truncated tag line keeps the tags that
/// actually organize the project; name breaks ties for a stable order.
pub fn group_by_tag(notes: &[Note]) -> Vec<TagGroup> {
    let mut by_tag: BTreeMap<&str, Vec<(String, u32)>> = BTreeMap::new();
    for note in notes {
        for tag in &note.tags {
            by_tag
                .entry(tag)
                .or_default()
                .push((note.name.clone(), note.tokens));
        }
    }
    let mut groups: Vec<_> = by_tag
        .into_iter()
        .map(|(tag, files)| TagGroup {
            tag: tag.to_owned(),
            files,
        })
        .collect();
    groups.sort_by(|a, b| b.files.len().cmp(&a.files.len()).then(a.tag.cmp(&b.tag)));
    groups
}

pub fn unreadable_warning(warnings: &[String]) -> Option<String> {
    (!warnings.is_empty()).then(|| format!("{UNREADABLE_PREFIX}{}", warnings.join(", ")))
}

/// Truncates on a codepoint boundary; a cut mid-sequence would hand the model
/// invalid UTF-8 to quote back.
pub fn cap(text: String, hint: &str) -> String {
    if text.len() <= MAX_FILE_BYTES {
        return text;
    }
    let mut cut = MAX_FILE_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n... (output truncated at {MAX_FILE_BYTES} bytes; {hint})",
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

pub fn encode_frontmatter(tags: &[String]) -> String {
    let yaml = serde_yaml::to_string(&serde_json::json!({ "tags": tags }))
        .unwrap_or_else(|_| "tags: []\n".into());
    format!("---\n{yaml}---\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::token_label;
    use test_case::test_case;

    #[test_case("Architecture", "architecture" ; "lowercased")]
    #[test_case("build-system", "build_system" ; "dash becomes underscore")]
    #[test_case("a  b", "a_b" ; "runs collapse")]
    #[test_case("--lead", "lead" ; "leading separators dropped")]
    #[test_case("trail--", "trail" ; "trailing separators dropped")]
    #[test_case("a.b.c", "a_b_c" ; "dots")]
    fn a_tag_is_coerced_into_snake_case(input: &str, expected: &str) {
        assert_eq!(normalize_tag(input).as_deref(), Some(expected));
    }

    #[test_case("" ; "empty")]
    #[test_case("---" ; "only separators")]
    #[test_case("  " ; "only whitespace")]
    fn a_tag_with_nothing_to_keep_is_rejected(input: &str) {
        assert!(normalize_tag(input).is_none());
    }

    #[test]
    fn a_long_tag_is_truncated() {
        let tag = normalize_tag(&"a".repeat(200)).unwrap();
        assert_eq!(tag.chars().count(), MAX_TAG_LEN);
    }

    #[test]
    fn duplicate_tags_collapse_but_stay_in_order() {
        let raw = ["b".to_owned(), "a".to_owned(), "B".to_owned()];
        let (normalized, ..) = normalize_tags(&raw);
        assert_eq!(normalized, vec!["b", "a"]);
    }

    #[test]
    fn a_write_refuses_a_tag_it_cannot_normalize() {
        let error = tags_for_write(&["ok".to_owned(), "###".to_owned()]).unwrap_err();
        assert!(error.starts_with(WRITE_REJECT_PREFIX), "{error}");
    }

    #[test]
    fn a_write_reports_the_tags_it_rewrote() {
        let (tags, note) = tags_for_write(&["Build-System".to_owned()]).unwrap();
        assert_eq!(tags, vec!["build_system"]);
        assert!(note.unwrap().contains("Build-System -> build_system"));
    }

    /// A filter tolerates junk the way a write cannot: the good tags still
    /// describe what the model wanted.
    #[test]
    fn a_filter_warns_about_junk_but_keeps_the_rest() {
        let (wanted, warning) = tags_for_filter(&["ok".to_owned(), "###".to_owned()]).unwrap();
        assert!(wanted.contains("ok"));
        assert!(warning.unwrap().starts_with(READ_REJECT_PREFIX));
    }

    #[test]
    fn a_filter_of_only_junk_is_an_error() {
        assert!(tags_for_filter(&["###".to_owned()]).is_err());
    }

    #[test_case("---\ntags: [a]\n---\nbody", Some("body") ; "well formed")]
    #[test_case("---\ntags: [a]\n---\n\n\nbody", Some("body") ; "blank lines after the fence")]
    fn frontmatter_is_split_from_the_body(input: &str, body: Option<&str>) {
        let (frontmatter, parsed) = parse_frontmatter(input);
        assert!(frontmatter.is_some());
        assert_eq!(Some(parsed), body);
    }

    #[test_case("no frontmatter here" ; "absent")]
    #[test_case("---\ntags: [a]\nnever closed" ; "unterminated")]
    fn content_without_usable_frontmatter_is_all_body(input: &str) {
        let (frontmatter, body) = parse_frontmatter(input);
        assert!(frontmatter.is_none());
        assert_eq!(body, input, "an unparsed fence must not eat the note");
    }

    #[test]
    fn frontmatter_round_trips_through_the_encoder() {
        let tags = vec!["a".to_owned(), "b".to_owned()];
        let encoded = format!("{}body", encode_frontmatter(&tags));
        let (frontmatter, body) = parse_frontmatter(&encoded);
        assert_eq!(tags_from_frontmatter(frontmatter.as_ref()), Some(tags));
        assert_eq!(body, "body");
    }

    #[test]
    fn an_empty_tag_list_round_trips_as_no_tags() {
        let encoded = format!("{}body", encode_frontmatter(&[]));
        let (frontmatter, _) = parse_frontmatter(&encoded);
        assert_eq!(tags_from_frontmatter(frontmatter.as_ref()), None);
    }

    #[test]
    fn a_single_string_tag_is_accepted() {
        let (frontmatter, _) = parse_frontmatter("---\ntags: solo\n---\nbody");
        assert_eq!(
            tags_from_frontmatter(frontmatter.as_ref()),
            Some(vec!["solo".to_owned()])
        );
    }

    fn write_note(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn an_untagged_note_falls_back_to_its_filename() {
        let temp = tempfile::tempdir().unwrap();
        write_note(temp.path(), "build-system.md", "no frontmatter");
        let (notes, warnings) = scan(temp.path(), &mut TagCache::default());
        assert!(warnings.is_empty());
        assert_eq!(notes[0].tags, vec!["build_system"]);
    }

    #[test]
    fn frontmatter_tags_win_over_the_filename() {
        let temp = tempfile::tempdir().unwrap();
        write_note(temp.path(), "notes.md", "---\ntags: [arch]\n---\nbody");
        let (notes, _) = scan(temp.path(), &mut TagCache::default());
        assert_eq!(notes[0].tags, vec!["arch"]);
    }

    #[test]
    fn groups_lead_with_the_most_used_tag() {
        let temp = tempfile::tempdir().unwrap();
        write_note(temp.path(), "a.md", "---\ntags: [common]\n---\nx");
        write_note(temp.path(), "b.md", "---\ntags: [common]\n---\nx");
        write_note(temp.path(), "c.md", "---\ntags: [rare]\n---\nx");
        let (notes, _) = scan(temp.path(), &mut TagCache::default());
        let groups = group_by_tag(&notes);
        assert_eq!(groups[0].tag, "common");
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[1].tag, "rare");
    }

    #[test]
    fn a_missing_directory_scans_as_empty() {
        let (notes, warnings) = scan(Path::new("/nonexistent/memories"), &mut TagCache::default());
        assert!(notes.is_empty() && warnings.is_empty());
    }

    /// Re-reading every note on every prompt build would make the tag line cost
    /// scale with the project's history, and would re-tokenize every body.
    /// Driven through the cache directly so the test controls the mtime
    /// instead of racing the filesystem clock.
    #[test]
    fn a_note_with_an_unchanged_stamp_is_not_reread() {
        let temp = tempfile::tempdir().unwrap();
        write_note(temp.path(), "a.md", "---\ntags: [first]\n---\nx");
        let stamp = SystemTime::UNIX_EPOCH;
        let size = std::fs::metadata(temp.path().join("a.md")).unwrap().len();
        let mut cache = TagCache::default();
        assert_eq!(
            cache
                .measure(temp.path(), "a.md", size, Some(stamp))
                .unwrap(),
            (vec!["first".to_owned()], 1)
        );

        write_note(
            temp.path(),
            "a.md",
            "---\ntags: [secnd]\n---\nlonger body here",
        );
        assert_eq!(
            cache
                .measure(temp.path(), "a.md", size, Some(stamp))
                .unwrap(),
            (vec!["first".to_owned()], 1),
            "same size and stamp must not trigger a re-read or a re-count"
        );
        assert_eq!(
            cache
                .measure(temp.path(), "a.md", size, Some(SystemTime::now()))
                .unwrap(),
            (
                vec!["secnd".to_owned()],
                estimate_tokens("longer body here")
            ),
            "a new stamp must invalidate both the tags and the count"
        );
    }

    /// Caching a failure would pin a stem tag past whatever caused it.
    #[test]
    fn an_unreadable_note_is_reported_and_not_cached() {
        let temp = tempfile::tempdir().unwrap();
        let mut cache = TagCache::default();
        let stamp = SystemTime::UNIX_EPOCH;
        assert!(
            cache
                .measure(temp.path(), "gone.md", 1, Some(stamp))
                .is_err()
        );
        write_note(temp.path(), "gone.md", "---\ntags: [back]\n---\nx");
        assert_eq!(
            cache
                .measure(temp.path(), "gone.md", 1, Some(stamp))
                .unwrap(),
            (vec!["back".to_owned()], 1)
        );
    }

    #[test]
    fn a_short_body_is_not_capped() {
        assert_eq!(cap("short".into(), CAP_HINT_REWRITE), "short");
    }

    #[test]
    fn an_oversized_body_is_capped_with_the_hint() {
        let capped = cap("x".repeat(MAX_FILE_BYTES + 100), CAP_HINT_REWRITE);
        assert!(capped.contains(CAP_HINT_REWRITE), "{capped}");
        assert!(capped.starts_with(&"x".repeat(MAX_FILE_BYTES)));
    }

    /// Cutting mid-codepoint would panic on the slice, and a byte-exact cap
    /// lands there for any multi-byte content.
    #[test]
    fn a_cap_never_splits_a_codepoint() {
        let capped = cap("é".repeat(MAX_FILE_BYTES), CAP_HINT_REWRITE);
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
