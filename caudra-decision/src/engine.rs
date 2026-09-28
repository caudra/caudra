use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_io::Timer;
use async_trait::async_trait;
use futures_lite::future;
use thiserror::Error;

use crate::question_set::{bounded_json, content_hash};
use crate::wire::{DecisionRequest, DecisionResponse, MAX_RESPONSE_BYTES};

pub const MAX_CACHE_ENTRIES: usize = 256;
pub const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DecisionError {
    #[error("decision engine is unreachable")]
    Unreachable,
    #[error("decision engine deadline exceeded")]
    Timeout,
    #[error("decision engine returned HTTP {status}")]
    Http { status: u16 },
    #[error("invalid decision response: {0}")]
    Invalid(&'static str),
    #[error("decision request rejected: {0}")]
    Rejected(&'static str),
}

#[async_trait]
pub trait DecisionEngine: Send + Sync {
    async fn decide(
        &self,
        request: &DecisionRequest,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError>;
}

#[async_trait]
impl<E: DecisionEngine + ?Sized> DecisionEngine for Arc<E> {
    async fn decide(
        &self,
        request: &DecisionRequest,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        self.as_ref().decide(request, deadline).await
    }
}

pub struct CachedDecisionEngine<E> {
    inner: E,
    capacity: usize,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
    bytes: usize,
}

impl<E: DecisionEngine> CachedDecisionEngine<E> {
    pub fn new(inner: E, capacity: usize) -> Self {
        Self {
            inner,
            capacity: capacity.min(MAX_CACHE_ENTRIES),
            cache: Mutex::new(Cache::default()),
        }
    }

    pub fn clear(&self) {
        *self.cache.lock().unwrap_or_else(|error| error.into_inner()) = Cache::default();
    }
}

#[async_trait]
impl<E: DecisionEngine> DecisionEngine for CachedDecisionEngine<E> {
    async fn decide(
        &self,
        request: &DecisionRequest,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        check_deadline(deadline)?;
        request.validate()?;
        let key = content_hash(request)?;
        check_deadline(deadline)?;
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries
            .get(&key)
            .cloned();
        if let Some(bytes) = cached {
            let mut response: DecisionResponse = serde_json::from_slice(&bytes)
                .map_err(|_| DecisionError::Invalid("cached response cannot be decoded"))?;
            response.cache_hit = true;
            check_deadline(deadline)?;
            return Ok(response);
        }
        let response = future::race(self.inner.decide(request, deadline), async {
            Timer::at(deadline).await;
            Err(DecisionError::Timeout)
        })
        .await?;
        check_deadline(deadline)?;
        response.validate_for(request)?;
        if self.capacity > 0 {
            let bytes = bounded_json(&response, MAX_RESPONSE_BYTES)
                .map_err(|_| DecisionError::Invalid("response exceeds the byte limit"))?;
            let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            if !cache.entries.contains_key(&key) {
                while cache.entries.len() >= self.capacity
                    || cache.bytes + bytes.len() > MAX_CACHE_BYTES
                {
                    let Some(oldest) = cache.order.pop_front() else {
                        break;
                    };
                    if let Some(old) = cache.entries.remove(&oldest) {
                        cache.bytes -= old.len();
                    }
                }
                cache.bytes += bytes.len();
                cache.order.push_back(key.clone());
                cache.entries.insert(key, bytes);
            }
        }
        check_deadline(deadline)?;
        Ok(response)
    }
}

pub(crate) fn check_deadline(deadline: Instant) -> Result<(), DecisionError> {
    if Instant::now() >= deadline {
        Err(DecisionError::Timeout)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use futures_lite::future;
    use serde_json::json;
    use test_case::test_case;

    use super::{
        CachedDecisionEngine, DecisionEngine, DecisionError, MAX_CACHE_BYTES, MAX_CACHE_ENTRIES,
    };
    use crate::wire::{
        DecisionRequest, DecisionResponse,
        tests::{request, response},
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct FakeEngine {
        calls: AtomicUsize,
        invalid: bool,
        fail: bool,
    }

    #[async_trait]
    impl DecisionEngine for FakeEngine {
        async fn decide(
            &self,
            _: &DecisionRequest,
            _: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(DecisionError::Unreachable);
            }
            let mut response = response();
            if self.invalid {
                response.answers.clear();
            }
            Ok(response)
        }
    }

    #[test]
    fn cache_keys_include_model_state_and_questions() {
        future::block_on(async {
            let cache = CachedDecisionEngine::new(FakeEngine::default(), MAX_CACHE_ENTRIES);
            let mut request = request();
            let deadline = Instant::now() + TEST_TIMEOUT;
            assert!(!cache.decide(&request, deadline).await.unwrap().cache_hit);
            assert!(cache.decide(&request, deadline).await.unwrap().cache_hit);
            request.model = "multilingual".into();
            assert!(!cache.decide(&request, deadline).await.unwrap().cache_hit);
            request.state = json!({"command":"git diff"});
            assert!(!cache.decide(&request, deadline).await.unwrap().cache_hit);
            request.questions.get_mut("writes").unwrap().instructions = json!("Does this write?");
            assert!(!cache.decide(&request, deadline).await.unwrap().cache_hit);
            assert_eq!(cache.inner.calls.load(Ordering::Relaxed), 4);
            cache.clear();
            assert!(!cache.decide(&request, deadline).await.unwrap().cache_hit);
        });
    }

    #[test_case(0; "disabled")]
    #[test_case(1; "evicted")]
    fn cache_capacity_is_bounded(capacity: usize) {
        future::block_on(async {
            let cache = CachedDecisionEngine::new(FakeEngine::default(), capacity);
            let first = request();
            let mut second = first.clone();
            second.state = json!("different");
            let deadline = Instant::now() + TEST_TIMEOUT;
            cache.decide(&first, deadline).await.unwrap();
            cache.decide(&second, deadline).await.unwrap();
            assert!(!cache.decide(&first, deadline).await.unwrap().cache_hit);
            let state = cache.cache.lock().unwrap();
            assert!(state.entries.len() <= capacity);
            assert!(state.bytes <= MAX_CACHE_BYTES);
        });
    }

    #[test_case(true, false; "invalid_response")]
    #[test_case(false, true; "transport_failure")]
    fn failures_are_not_cached(invalid: bool, fail: bool) {
        future::block_on(async {
            let cache = CachedDecisionEngine::new(
                FakeEngine {
                    invalid,
                    fail,
                    ..FakeEngine::default()
                },
                1,
            );
            let deadline = Instant::now() + TEST_TIMEOUT;
            assert!(cache.decide(&request(), deadline).await.is_err());
            assert!(cache.decide(&request(), deadline).await.is_err());
            assert_eq!(cache.inner.calls.load(Ordering::Relaxed), 2);
            assert!(cache.cache.lock().unwrap().entries.is_empty());
        });
    }

    #[test]
    fn expired_deadline_does_not_use_cached_result() {
        future::block_on(async {
            let cache = CachedDecisionEngine::new(FakeEngine::default(), 1);
            cache
                .decide(&request(), Instant::now() + TEST_TIMEOUT)
                .await
                .unwrap();
            assert_eq!(
                cache.decide(&request(), Instant::now()).await,
                Err(DecisionError::Timeout)
            );
            assert_eq!(cache.inner.calls.load(Ordering::Relaxed), 1);
        });
    }
}
