//! Host-owned bounds for the code-graph tools.
//!
//! Every field here is startup configuration. None of it is reachable from tool input, for the same
//! reason `IndexLimits` is not: a model that can raise a ceiling can raise it until the work no
//! longer fits, and the ceiling stops being a bound and becomes a suggestion.

use std::time::Duration;

use workcell_code_graph::{CacheLimits, ExtractLimits, IngestLimits};

/// Bounds on crawling, extraction, ranking, and result size.
#[derive(Clone, Copy, Debug)]
pub struct CodeGraphLimits {
    /// Source files admitted to one map.
    pub max_files: usize,
    /// Directory entries the traversal may examine.
    pub max_traversal_entries: usize,
    /// Largest single source file admitted.
    pub max_source_bytes: usize,
    /// Total source bytes read for one map.
    pub max_total_bytes: usize,
    /// Wall-clock ceiling on the crawl.
    pub crawl_deadline: Duration,
    /// Symbols returned when the caller names no limit.
    pub default_result_limit: usize,
    /// Symbols returned however large a limit the caller names.
    ///
    /// A caller-supplied limit narrows the result; it can never widen it past this.
    pub max_result_limit: usize,
    /// Bytes of source `code_expand` may return for one symbol.
    pub max_expand_bytes: usize,
    /// Did-you-mean candidates offered for an unknown selector.
    pub max_suggestions: usize,
    pub ingest: IngestLimits,
    pub cache: CacheLimits,
}

impl Default for CodeGraphLimits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_traversal_entries: 200_000,
            max_source_bytes: 4 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            crawl_deadline: Duration::from_secs(60),
            default_result_limit: 200,
            max_result_limit: 2_000,
            max_expand_bytes: 32 * 1024,
            max_suggestions: 5,
            ingest: IngestLimits::default(),
            cache: CacheLimits::default(),
        }
    }
}

impl CodeGraphLimits {
    /// Clamps a caller-supplied result limit.
    ///
    /// `None` means the caller expressed no preference and gets the default, which is not the
    /// maximum: a tool that returns its ceiling by default spends the whole result budget before
    /// the caller has said what they want.
    #[must_use]
    pub fn resolve_limit(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.default_result_limit)
            .clamp(1, self.max_result_limit)
    }

    #[must_use]
    pub const fn extract(&self) -> ExtractLimits {
        self.ingest.extract
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_limit_is_the_default_not_the_ceiling() {
        let limits = CodeGraphLimits::default();
        assert_eq!(limits.resolve_limit(None), limits.default_result_limit);
        assert!(limits.default_result_limit < limits.max_result_limit);
    }

    #[test]
    fn a_caller_limit_narrows_but_never_widens() {
        let limits = CodeGraphLimits::default();
        assert_eq!(limits.resolve_limit(Some(10)), 10);
        assert_eq!(
            limits.resolve_limit(Some(usize::MAX)),
            limits.max_result_limit,
            "input must not be able to raise a host ceiling"
        );
        assert_eq!(limits.resolve_limit(Some(0)), 1);
    }
}
