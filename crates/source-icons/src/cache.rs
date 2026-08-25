use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lru::LruCache;

const POSITIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const NEGATIVE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
struct TimedEntry<T> {
    value: Option<T>,
    expires_at: Instant,
}

pub(crate) enum CacheRead<T> {
    Hit(Option<T>),
    Miss,
}

pub(crate) struct IconCaches {
    probes: Mutex<LruCache<String, TimedEntry<String>>>,
    encoded: Mutex<LruCache<String, TimedEntry<String>>>,
}

impl Default for IconCaches {
    fn default() -> Self {
        Self::new(2_000, 1_000)
    }
}

impl IconCaches {
    pub(crate) fn new(probe_capacity: usize, encoded_capacity: usize) -> Self {
        Self {
            probes: Mutex::new(LruCache::new(nonzero(probe_capacity))),
            encoded: Mutex::new(LruCache::new(nonzero(encoded_capacity))),
        }
    }

    pub(crate) fn get_probe(&self, key: &str) -> CacheRead<String> {
        get(&mut lock(&self.probes), key)
    }

    pub(crate) fn put_probe(&self, key: String, value: Option<String>) {
        put(&mut lock(&self.probes), key, value);
    }

    pub(crate) fn get_encoded(&self, key: &str) -> CacheRead<String> {
        get(&mut lock(&self.encoded), key)
    }

    pub(crate) fn put_encoded(&self, key: String, value: Option<String>) {
        put(&mut lock(&self.encoded), key, value);
    }

    pub(crate) fn clear(&self) {
        lock(&self.probes).clear();
        lock(&self.encoded).clear();
    }
}

fn get(cache: &mut LruCache<String, TimedEntry<String>>, key: &str) -> CacheRead<String> {
    if cache
        .peek(key)
        .is_some_and(|entry| entry.expires_at <= Instant::now())
    {
        cache.pop(key);
        return CacheRead::Miss;
    }
    cache
        .get(key)
        .map_or(CacheRead::Miss, |entry| CacheRead::Hit(entry.value.clone()))
}

fn put(cache: &mut LruCache<String, TimedEntry<String>>, key: String, value: Option<String>) {
    let ttl = if value.is_some() {
        POSITIVE_TTL
    } else {
        NEGATIVE_TTL
    };
    cache.put(
        key,
        TimedEntry {
            value,
            expires_at: Instant::now() + ttl,
        },
    );
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value.max(1)).expect("value was clamped to at least one")
}
