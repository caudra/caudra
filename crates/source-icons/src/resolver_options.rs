use std::time::Duration;

use crate::resolver::ResolveSourceIconOptions;
use url::Url;

pub(crate) const MAX_URL_BYTES: usize = 8 * 1024;
pub(crate) const MAX_PATH_SEGMENTS: usize = 128;
const MAX_CANDIDATES: usize = 64;
const MAX_PROBE_BATCH_SIZE: usize = 8;
const MAX_REQUESTS: usize = 128;
const MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn normalized_options(
    mut options: ResolveSourceIconOptions,
) -> ResolveSourceIconOptions {
    options.timeout = nonzero_duration(options.timeout, Duration::from_millis(1_500));
    options.probe_timeout = nonzero_duration(options.probe_timeout, Duration::from_millis(400));
    if options.max_icon_bytes == 0 {
        options.max_icon_bytes = 1_000_000;
    }
    if options.max_probe_bytes == 0 {
        options.max_probe_bytes = 2_048;
    }
    if options.max_html_bytes == 0 {
        options.max_html_bytes = 512 * 1_024;
    }
    if options.max_candidates == 0 {
        options.max_candidates = 20;
    }
    options.max_candidates = options.max_candidates.min(MAX_CANDIDATES);
    options.total_timeout =
        nonzero_duration(options.total_timeout, Duration::from_secs(5)).min(MAX_TOTAL_TIMEOUT);
    if options.max_requests == 0 {
        options.max_requests = 24;
    }
    options.max_requests = options.max_requests.min(MAX_REQUESTS);
    if options.probe_batch_size == 0 {
        options.probe_batch_size = 3;
    }
    options.probe_batch_size = options
        .probe_batch_size
        .min(MAX_PROBE_BATCH_SIZE)
        .min(options.max_candidates);
    if options.data_url_soft_limit == 0 {
        options.data_url_soft_limit = 2_048;
    }
    options
}

pub(crate) fn url_is_within_limits(url: &Url) -> bool {
    url.as_str().len() <= MAX_URL_BYTES
        && url.path_segments().is_none_or(|segments| {
            segments.take(MAX_PATH_SEGMENTS + 1).count() <= MAX_PATH_SEGMENTS
        })
}

fn nonzero_duration(value: Duration, fallback: Duration) -> Duration {
    if value.is_zero() { fallback } else { value }
}
