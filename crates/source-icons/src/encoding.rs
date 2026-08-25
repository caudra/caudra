use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};

use tokio::sync::Semaphore;
use url::Url;

use crate::budget::ResolutionBudget;
use crate::cache::CacheRead;
use crate::icon_fetch::{FetchOutcome, FetchSpec, definitive_http_failure};
use crate::image_normalize::{OutputProfile, normalize_to_data_url, sniff_icon_kind};
use crate::resolver::{
    ResolveSourceIconOptions, ResolvedSourceIcon, SourceIconCacheInfo, SourceIconError,
    SourceIconResolver, SourceIconSource,
};

const MAX_BLOCKING_IMAGE_JOBS: usize = 4;
static IMAGE_JOBS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_BLOCKING_IMAGE_JOBS)));

impl SourceIconResolver {
    pub(crate) async fn encode_icon(
        &self,
        icon_url: Url,
        source: SourceIconSource,
        options: &ResolveSourceIconOptions,
        budget: &ResolutionBudget,
        cache: &mut SourceIconCacheInfo,
    ) -> Result<Option<ResolvedSourceIcon>, SourceIconError> {
        let profile = output_profile(options);
        let key = encoded_cache_key(&icon_url, &profile, options.max_icon_bytes);
        match self.caches.get_encoded(&key) {
            CacheRead::Hit(value) => {
                cache.encoded.hits += 1;
                return Ok(value.map(|icon_data_url| ResolvedSourceIcon {
                    icon_url: icon_url.to_string(),
                    icon_data_url,
                    icon_source: source,
                    cache: *cache,
                }));
            }
            CacheRead::Miss => cache.encoded.misses += 1,
        }
        let response = self
            .fetch(
                FetchSpec {
                    url: &icon_url,
                    timeout: options.timeout,
                    max_body_bytes: options.max_icon_bytes,
                    accept: "image/*,*/*;q=0.1",
                    range: None,
                },
                options,
                budget,
            )
            .await?;
        let response = match response {
            FetchOutcome::Response(response) => *response,
            FetchOutcome::DefinitiveFailure => {
                self.write_encoded(key, None, cache);
                return Ok(None);
            }
            FetchOutcome::TransientFailure => return Ok(None),
        };
        if !response.status.is_success() {
            if definitive_http_failure(response.status) {
                self.write_encoded(key, None, cache);
            }
            return Ok(None);
        }
        if response.truncated || sniff_icon_kind(&response.body).is_none() {
            self.write_encoded(key, None, cache);
            return Ok(None);
        }

        let bytes = response.body.to_vec();
        let worker_profile = profile.clone();
        let max_input_bytes = options.max_icon_bytes;
        let permit = tokio::select! {
            biased;
            () = options.cancellation.cancelled() => return Err(SourceIconError::Cancelled),
            result = tokio::time::timeout_at(
                budget.deadline(),
                Arc::clone(&IMAGE_JOBS).acquire_owned(),
            ) => match result {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) | Err(_) => return Ok(None),
            },
        };
        // The permit moves into the worker, so a caller timeout cannot release
        // capacity while detached spawn_blocking work is still running.
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            normalize_to_data_url(&bytes, &worker_profile, max_input_bytes).ok()
        });
        let data_url = tokio::select! {
            biased;
            () = options.cancellation.cancelled() => return Err(SourceIconError::Cancelled),
            result = tokio::time::timeout_at(budget.deadline(), worker) => match result {
                Ok(Ok(data_url)) => data_url,
                Ok(Err(_)) | Err(_) => return Ok(None),
            },
        };
        self.write_encoded(key, data_url.clone(), cache);
        Ok(data_url.map(|icon_data_url| ResolvedSourceIcon {
            icon_url: icon_url.to_string(),
            icon_data_url,
            icon_source: source,
            cache: *cache,
        }))
    }

    fn write_encoded(&self, key: String, value: Option<String>, cache: &mut SourceIconCacheInfo) {
        cache.encoded.writes += 1;
        self.caches.put_encoded(key, value);
    }
}

fn output_profile(options: &ResolveSourceIconOptions) -> OutputProfile {
    OutputProfile {
        sizes: normalized_descending(&options.output_sizes, &[24_u32, 20, 16], |value| value > 0),
        qualities: normalized_descending(
            &options.output_png_qualities,
            &[90_u8, 80, 70],
            |value| value > 0,
        )
        .into_iter()
        .map(|quality| quality.min(100))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .rev()
        .collect(),
        soft_limit: options.data_url_soft_limit,
    }
}

fn normalized_descending<T: Copy + Ord>(
    values: &[T],
    fallback: &[T],
    valid: impl Fn(T) -> bool,
) -> Vec<T> {
    let source = if values.iter().copied().any(&valid) {
        values
    } else {
        fallback
    };
    source
        .iter()
        .copied()
        .filter(|value| valid(*value))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn encoded_cache_key(url: &Url, profile: &OutputProfile, max_input_bytes: usize) -> String {
    format!(
        "{}\0sizes={}\0qualities={}\0soft={}\0input={}",
        url,
        profile
            .sizes
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(","),
        profile
            .qualities
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(","),
        profile.soft_limit,
        max_input_bytes,
    )
}
