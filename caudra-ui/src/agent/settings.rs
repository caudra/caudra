use std::sync::Arc;

use arc_swap::ArcSwapOption;
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::{AgentConfig, McpHandle};

/// Settings read from files that changed while a session runs.
#[derive(Clone)]
pub(crate) struct Settings {
    pub(crate) config: AgentConfig,
    pub(crate) prompt_profiles: Arc<PromptProfileCatalog>,
    /// The MCP manager, which a session that started without one adopts.
    pub(crate) mcp: Option<McpHandle>,
}

/// Where the event loop leaves [`Settings`] for one session's loop, which
/// adopts the latest when its next turn starts.
#[derive(Clone, Default)]
pub(crate) struct LiveSettings(Arc<ArcSwapOption<Settings>>);

impl LiveSettings {
    pub(crate) fn publish(&self, settings: Settings) {
        self.0.store(Some(Arc::new(settings)));
    }

    pub(super) fn take(&self) -> Option<Settings> {
        self.0.swap(None).map(Arc::unwrap_or_clone)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_agent::AgentConfig;

    use super::{LiveSettings, Settings};

    const OLDER_LIMIT: usize = 1_000;
    const NEWER_LIMIT: usize = 2_000;

    fn settings(max_output_lines: usize) -> Settings {
        Settings {
            config: AgentConfig {
                max_output_lines,
                ..AgentConfig::default()
            },
            prompt_profiles: Arc::default(),
            mcp: None,
        }
    }

    #[test]
    fn the_latest_publication_is_adopted_once() {
        let live = LiveSettings::default();
        live.publish(settings(OLDER_LIMIT));
        live.publish(settings(NEWER_LIMIT));

        let adopted = live.take().unwrap();

        assert_eq!(adopted.config.max_output_lines, NEWER_LIMIT);
        assert!(live.take().is_none());
    }
}
