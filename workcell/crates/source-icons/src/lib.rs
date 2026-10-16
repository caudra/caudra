#![forbid(unsafe_code)]

//! Safe favicon discovery, verification, conversion, and process-local caching.
//!
//! Remote icon bytes are never returned directly. Discovery is followed by a
//! bounded download, magic-byte allowlist, dimension-limited decode, and PNG
//! normalization. Network safety is delegated to `workcell-net`, so HTML links,
//! fallback guesses, DNS answers, and every redirect share one SSRF policy.

mod budget;
mod cache;
mod candidates;
mod encoding;
mod html;
mod icon_fetch;
mod image_normalize;
mod probe;
mod resolver;
mod resolver_options;
mod svg_rasterize;

#[cfg(test)]
mod image_normalize_tests;
#[cfg(test)]
mod resolver_tests;
#[cfg(test)]
mod svg_rasterize_tests;

pub use resolver::{
    CacheCounts, ResolveSourceIconOptions, ResolvedSourceIcon, SourceIconCacheInfo,
    SourceIconError, SourceIconResolver, SourceIconSource, clear_source_icon_caches,
    resolve_source_icon,
};
