//! A bounded, content-addressed cache of per-file extraction results.
//!
//! Extraction is the expensive stage: parsing dominates every other cost in the pipeline, and a
//! repository map is usually recomputed after a handful of files changed. Caching the parse means
//! the second run pays for the files that moved and nothing else.
//!
//! # Why the key is a content digest and not a stat triple
//!
//! The usual design keys on `(path, length, mtime, ctime)` and falls back to hashing only when the
//! mtime is too recent to be trusted, because the cache sits underneath the file read and its whole
//! purpose is to avoid performing one.
//!
//! This cache sits above the read. Nothing in this crate touches the filesystem; a caller hands in
//! source text it has already read and already authorized. In that position a stat triple could
//! only ever save the hash of bytes that are already in memory, which costs a small fraction of the
//! parse it is protecting, while reintroducing every way a stat triple can agree about a file that
//! changed. Hashing unconditionally is both simpler and strictly safer here, and it removes the
//! racy-timestamp rule along with the failure it exists to patch.
//!
//! A cache that avoids the read belongs to whichever layer performs the read, and would key on
//! whatever that layer can observe without opening the file.

use std::{num::NonZeroUsize, sync::Arc};

use lru::LruCache;
use sha2::{Digest as _, Sha256};
use workcell_source_languages::Language;

use crate::extract::{
    ExtractItem, ExtractLimits, FileFacts, extract, extract_batch, worker_threads,
};

/// A content digest of one file's source text.
type Digest = [u8; 32];

/// Bounds on retained extraction results.
///
/// Two ceilings, because either alone is escapable: an entry count says nothing about a repository
/// of very large files, and a byte ceiling alone permits unbounded per-entry bookkeeping for a
/// repository of empty ones.
#[derive(Clone, Copy, Debug)]
pub struct CacheLimits {
    pub max_entries: NonZeroUsize,
    pub max_bytes: usize,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            max_entries: NonZeroUsize::new(4_096).unwrap_or(NonZeroUsize::MIN),
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Hits, misses, and evictions since construction. Observability only; nothing branches on these.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

struct Entry {
    digest: Digest,
    facts: Arc<FileFacts>,
    /// Retained bytes attributed to this entry, as estimated at insertion.
    weight: usize,
}

/// A bounded cache of extraction results, keyed by path and validated by content digest.
///
/// Not `Sync`. A caller that needs sharing owns the lock, which keeps the eviction accounting a
/// single-threaded invariant rather than something to reason about under contention.
pub struct FactsCache {
    entries: LruCache<String, Entry>,
    limits: CacheLimits,
    bytes: usize,
    stats: CacheStats,
}

impl FactsCache {
    #[must_use]
    pub fn new(limits: CacheLimits) -> Self {
        Self {
            entries: LruCache::new(limits.max_entries),
            limits,
            bytes: 0,
            stats: CacheStats::default(),
        }
    }

    #[must_use]
    pub const fn stats(&self) -> CacheStats {
        self.stats
    }

    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops every entry.
    ///
    /// The caller invokes this when the root changes. Paths are root-relative, so the same key can
    /// name different content under a different root, and nothing in the key would reveal that.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Returns the extraction result for `source`, extracting only on a miss.
    ///
    /// `None` propagates an extraction failure and is never cached: a failure is a property of the
    /// parser run, not of the content, and caching it would make one bad run permanent.
    pub fn facts(
        &mut self,
        path: &str,
        source: &str,
        language: Language,
        limits: ExtractLimits,
    ) -> Option<Arc<FileFacts>> {
        let digest: Digest = Sha256::digest(source.as_bytes()).into();
        if let Some(entry) = self.entries.get(path)
            && entry.digest == digest
        {
            self.stats.hits += 1;
            return Some(Arc::clone(&entry.facts));
        }
        self.stats.misses += 1;

        let facts = Arc::new(extract(source, language, limits)?);
        self.store(path, digest, &facts);
        Some(facts)
    }

    /// [`FactsCache::facts`] for a whole batch, extracting the misses together.
    ///
    /// The lookups stay sequential because they are a hash and a map probe; only the misses are
    /// worth a thread. Every result keeps its input position.
    #[must_use]
    pub fn facts_batch(
        &mut self,
        items: &[ExtractItem<'_>],
        limits: ExtractLimits,
        thread_ceiling: usize,
    ) -> Vec<Option<Arc<FileFacts>>> {
        let mut resolved: Vec<Option<Arc<FileFacts>>> = Vec::with_capacity(items.len());
        let mut misses: Vec<usize> = Vec::new();
        let mut digests: Vec<Digest> = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let digest: Digest = Sha256::digest(item.source.as_bytes()).into();
            let hit = self
                .entries
                .get(item.path)
                .filter(|entry| entry.digest == digest)
                .map(|entry| Arc::clone(&entry.facts));
            if hit.is_some() {
                self.stats.hits += 1;
            } else {
                self.stats.misses += 1;
                misses.push(index);
            }
            digests.push(digest);
            resolved.push(hit);
        }

        let pending: Vec<ExtractItem<'_>> = misses.iter().map(|index| items[*index]).collect();
        let threads = worker_threads(thread_ceiling, pending.len());
        let extracted = extract_batch(&pending, limits, threads);

        // Insertion order follows input order, not completion order, so the eviction the byte
        // ceiling forces is the same at any thread count.
        for (index, facts) in misses.into_iter().zip(extracted) {
            let Some(facts) = facts else { continue };
            let facts = Arc::new(facts);
            self.store(items[index].path, digests[index], &facts);
            resolved[index] = Some(facts);
        }
        resolved
    }

    /// Retains one extraction result, replacing any previous entry for the same path.
    fn store(&mut self, path: &str, digest: Digest, facts: &Arc<FileFacts>) {
        let weight = weigh(facts);
        if let Some(previous) = self.entries.pop(path) {
            self.bytes = self.bytes.saturating_sub(previous.weight);
        }
        self.entries.put(
            path.to_owned(),
            Entry {
                digest,
                facts: Arc::clone(facts),
                weight,
            },
        );
        self.bytes = self.bytes.saturating_add(weight);
        self.evict_to_bytes();
    }

    /// Evicts least-recently-used entries until the byte ceiling holds.
    ///
    /// The entry just inserted can itself be evicted, which is deliberate: a single file larger
    /// than the whole ceiling must not be allowed to sit above it.
    fn evict_to_bytes(&mut self) {
        while self.bytes > self.limits.max_bytes {
            let Some((_, evicted)) = self.entries.pop_lru() else {
                // Nothing left to evict. The accounting cannot exceed a ceiling with no entries.
                self.bytes = 0;
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted.weight);
            self.stats.evictions += 1;
        }
    }
}

/// Estimated retained bytes for one extraction result.
///
/// An estimate, and named as one. It counts the heap the strings own plus a fixed charge per record
/// for the fields beside them, which is what makes the byte ceiling track the shape of real
/// content rather than a record count wearing a byte label.
fn weigh(facts: &FileFacts) -> usize {
    const DEFINITION_OVERHEAD: usize = 128;
    const REFERENCE_OVERHEAD: usize = 64;

    let definitions: usize = facts
        .definitions
        .iter()
        .map(|definition| {
            DEFINITION_OVERHEAD
                + definition.name.len()
                + definition
                    .documentation
                    .as_ref()
                    .map_or(0, std::string::String::len)
        })
        .sum();
    let references: usize = facts
        .references
        .iter()
        .map(|reference| {
            REFERENCE_OVERHEAD
                + reference.name.len()
                + reference
                    .qualifier
                    .as_ref()
                    .map_or(0, std::string::String::len)
        })
        .sum();
    definitions + references
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "fn alpha() { beta(); }\nfn beta() {}\n";

    fn cache() -> FactsCache {
        FactsCache::new(CacheLimits::default())
    }

    fn facts(cache: &mut FactsCache, path: &str, source: &str) -> Arc<FileFacts> {
        cache
            .facts(path, source, Language::Rust, ExtractLimits::default())
            .expect("rust extracts")
    }

    #[test]
    fn a_second_call_on_unchanged_content_is_a_hit() {
        let mut cache = cache();
        let cold = facts(&mut cache, "src/a.rs", SOURCE);
        let warm = facts(&mut cache, "src/a.rs", SOURCE);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);
        assert!(
            Arc::ptr_eq(&cold, &warm),
            "a hit should hand back the retained result, not an equal one"
        );
    }

    #[test]
    fn warm_facts_are_identical_to_cold_facts() {
        // The gate that makes the cache safe to enable by default. A cache that is merely fast is
        // worthless if the answer it returns differs in any observable way from the answer a cold
        // process computes, and the difference is invisible in every test that only checks speed.
        let mut cold = cache();
        let mut warm = cache();

        let sources = [
            ("src/a.rs", "fn alpha() { beta(); }"),
            ("src/b.rs", "struct Beta;\nimpl Beta { fn beta(&self) {} }"),
            (
                "src/c.rs",
                "mod inner { pub fn gamma() {} }\nfn use_it() { inner::gamma(); }",
            ),
        ];

        // Prime the warm cache, then evict nothing and ask again.
        for (path, source) in sources {
            let _ = facts(&mut warm, path, source);
        }

        for (path, source) in sources {
            let from_cold = facts(&mut cold, path, source);
            let from_warm = facts(&mut warm, path, source);
            assert_eq!(
                format!("{from_cold:?}"),
                format!("{from_warm:?}"),
                "warm and cold extraction of {path} must be identical"
            );
        }
        assert_eq!(warm.stats().hits, 3, "the warm run must actually have hit");
        assert_eq!(
            cold.stats().hits,
            0,
            "the cold run must actually have missed"
        );
    }

    #[test]
    fn changed_content_at_the_same_path_invalidates() {
        let mut cache = cache();
        let before = facts(&mut cache, "src/a.rs", "fn alpha() {}");
        let after = facts(&mut cache, "src/a.rs", "fn renamed() {}");
        assert_eq!(cache.stats().hits, 0, "changed content is not a hit");
        assert_eq!(before.definitions[0].name, "alpha");
        assert_eq!(after.definitions[0].name, "renamed");
        assert_eq!(
            cache.len(),
            1,
            "the stale entry is replaced, not accumulated"
        );
    }

    #[test]
    fn reverting_content_is_a_hit_again() {
        // Content addressing, not generation counting: a revert is the same content and must not
        // pay for a reparse.
        let mut cache = cache();
        let _ = facts(&mut cache, "src/a.rs", "fn alpha() {}");
        let _ = facts(&mut cache, "src/a.rs", "fn renamed() {}");
        let _ = facts(&mut cache, "src/a.rs", "fn renamed() {}");
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn the_same_content_at_a_different_path_is_a_separate_entry() {
        // Paths are part of the key on purpose. Two files with identical text still resolve
        // differently, because the resolution ladder starts from the defining file.
        let mut cache = cache();
        let _ = facts(&mut cache, "src/a.rs", SOURCE);
        let _ = facts(&mut cache, "src/b.rs", SOURCE);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.stats().hits, 0);
    }

    #[test]
    fn the_entry_ceiling_evicts_least_recently_used() {
        let limits = CacheLimits {
            max_entries: NonZeroUsize::new(2).expect("nonzero"),
            ..CacheLimits::default()
        };
        let mut cache = FactsCache::new(limits);
        let _ = facts(&mut cache, "a.rs", "fn a() {}");
        let _ = facts(&mut cache, "b.rs", "fn b() {}");
        let _ = facts(&mut cache, "a.rs", "fn a() {}"); // a is now the most recent.
        let _ = facts(&mut cache, "c.rs", "fn c() {}"); // evicts b.
        assert_eq!(cache.len(), 2);

        let hits = cache.stats().hits;
        let _ = facts(&mut cache, "a.rs", "fn a() {}");
        assert_eq!(cache.stats().hits, hits + 1, "a survived");
        let _ = facts(&mut cache, "b.rs", "fn b() {}");
        assert_eq!(cache.stats().hits, hits + 1, "b was evicted");
    }

    #[test]
    fn the_byte_ceiling_holds_and_is_not_escaped_by_one_large_file() {
        let limits = CacheLimits {
            max_bytes: 512,
            ..CacheLimits::default()
        };
        let mut cache = FactsCache::new(limits);
        let large: String = (0..200)
            .map(|index| format!("fn function_with_a_long_name_{index}() {{}}\n"))
            .collect();
        let _ = facts(&mut cache, "large.rs", &large);
        assert!(
            cache.retained_bytes() <= 512,
            "an entry larger than the ceiling must not be retained above it"
        );
        assert!(cache.stats().evictions > 0, "the eviction must be recorded");
    }

    #[test]
    fn clearing_drops_every_entry_and_its_accounting() {
        let mut cache = cache();
        let _ = facts(&mut cache, "src/a.rs", SOURCE);
        assert!(cache.retained_bytes() > 0);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.retained_bytes(), 0);
        let _ = facts(&mut cache, "src/a.rs", SOURCE);
        assert_eq!(cache.stats().hits, 0, "a cleared entry is not a hit");
    }
}
