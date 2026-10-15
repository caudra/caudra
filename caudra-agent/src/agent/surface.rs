//! The prefix every request of a conversation shares, fixed: the tool array,
//! then the system prompt.
//!
//! Providers cache a request's prefix in order, tools first, so rebuilding
//! either half from configuration that changed while a session runs would
//! re-cache the whole conversation. Both stay as they were fixed, and how the
//! configuration differs from them reaches the model as a standing reminder.
//! They are rebuilt only where the cache is cold anyway: when the conversation
//! was replaced, when what the prefix is keyed on changed, and once the
//! provider's cache has expired.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::{Duration, Instant};

use caudra_providers::{Message, ThinkingConfig};
use serde_json::Value;

use crate::AgentMode;
use crate::agent::last_announced;
use crate::prompt::{
    CHANGES_SLOT, CONFIGURATION_CHANGED_MARKER, CONFIGURATION_CHANGED_PROMPT,
    CONFIGURATION_RESTORED_PROMPT,
};
use crate::tools::DeferredTool;
use crate::tools::deferral::summary;
use crate::tools::native::plan::PlanTarget;

const REMOVED_HEADING: &str = "Declared tools that no longer exist; calls to them fail:";
const ADDED_HEADING: &str = "Tools that exist now but are not declared; they become callable once the definitions are rebuilt:";
const CHANGED_HEADING: &str =
    "Declared tools whose definition changed; calls follow the current definition:";
const DESCRIPTION_KEY: &str = "description";
const SYSTEM_HEADING: &str = "The system prompt changed. The diff below is authoritative: apply it to what the system prompt says, and follow the result.";
const SYSTEM_DISPLAY_PATH: &str = "system prompt";

/// What a prefix is built from that changes only by the user's hand, so a
/// request under another key misses the cache anyway.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefixKey {
    pub model: String,
    pub chat_model: String,
    pub thinking: ThinkingConfig,
    pub mode: AgentMode,
    pub plan: Option<PlanTarget>,
    pub cwd: String,
}

/// One request prefix, and every definition it could declare.
pub struct Surface {
    pub system: String,
    pub tools: Value,
    pub deferred: Vec<DeferredTool>,
    /// By the name the model calls each one, whether declared or deferred.
    /// What a difference is measured on, so a `tool_search` load is never one.
    definitions: BTreeMap<String, Value>,
}

impl Surface {
    pub fn new(system: String, tools: Value, deferred: Vec<DeferredTool>) -> Self {
        let definitions = tools
            .as_array()
            .into_iter()
            .flatten()
            .chain(deferred.iter().map(|tool| &tool.definition))
            .filter_map(|definition| {
                Some((definition["name"].as_str()?.to_owned(), definition.clone()))
            })
            .collect();
        Self {
            system,
            tools,
            deferred,
            definitions,
        }
    }

    fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.tools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["name"].as_str())
            .chain(self.deferred.iter().map(|tool| tool.name.as_ref()))
    }
}

/// What [`SurfaceBaseline::settle`] decided for one turn.
pub struct Settled<'a> {
    /// The prefix the turn's requests carry.
    pub surface: &'a Surface,
    /// Built-in tools `surface` declares that the configuration no longer
    /// has, which the turn must not run.
    pub retired: Vec<String>,
    /// The `# Configuration changed` block the turn carries, if any.
    pub notice: Option<String>,
}

/// `K` is what the prefix is keyed on, a [`PrefixKey`] outside tests.
pub struct SurfaceBaseline<K = PrefixKey> {
    fixed: Option<Fixed<K>>,
    /// When the last request on the fixed prefix finished, which is when the
    /// provider's cache began to expire.
    used: Option<Instant>,
}

struct Fixed<K> {
    surface: Surface,
    key: K,
    epoch: u64,
}

impl<K> Default for SurfaceBaseline<K> {
    fn default() -> Self {
        Self {
            fixed: None,
            used: None,
        }
    }
}

impl<K: PartialEq> SurfaceBaseline<K> {
    /// Whether the next request rebuilds the prefix: nothing is fixed yet,
    /// `key` moved, the history `epoch` moved because the conversation was
    /// replaced, or `cache_ttl` passed since the last request.
    pub fn stale(&self, key: &K, epoch: u64, cache_ttl: Option<Duration>, now: Instant) -> bool {
        let Some(fixed) = &self.fixed else {
            return true;
        };
        fixed.key != *key
            || fixed.epoch != epoch
            || cache_ttl
                .zip(self.used)
                .is_some_and(|(ttl, used)| now.saturating_duration_since(used) >= ttl)
    }

    /// Fixes `live` when `rebuild`, and otherwise keeps the prefix already
    /// fixed and measures how `live` differs from it. `history` decides
    /// whether a prefix that matches again needs an earlier difference
    /// withdrawn.
    pub fn settle(
        &mut self,
        live: Surface,
        rebuild: bool,
        key: K,
        epoch: u64,
        history: &[Message],
    ) -> Settled<'_> {
        let (fixed, changes, retired) = match self.fixed.take() {
            Some(fixed) if !rebuild => {
                let changes = changes(&fixed.surface, &live);
                let retired = fixed
                    .surface
                    .builtin_names()
                    .filter(|name| !live.definitions.contains_key(*name))
                    .map(str::to_owned)
                    .collect();
                (fixed, changes, retired)
            }
            _ => (
                Fixed {
                    surface: live,
                    key,
                    epoch,
                },
                None,
                Vec::new(),
            ),
        };
        let notice = match changes {
            Some(changes) => Some(CONFIGURATION_CHANGED_PROMPT.replace(CHANGES_SLOT, &changes)),
            None => last_announced(history, &[CONFIGURATION_CHANGED_MARKER])
                .is_some_and(|text| text != CONFIGURATION_RESTORED_PROMPT)
                .then(|| CONFIGURATION_RESTORED_PROMPT.to_owned()),
        };
        let fixed = self.fixed.insert(fixed);
        Settled {
            surface: &fixed.surface,
            retired,
            notice,
        }
    }

    /// Records that a request on the fixed prefix just finished.
    pub fn touch(&mut self, now: Instant) {
        self.used = Some(now);
    }
}

/// Everything `live` says differently from `fixed`, in a fixed order so the
/// same difference renders the same block and is announced once.
fn changes(fixed: &Surface, live: &Surface) -> Option<String> {
    let mut sections = Vec::new();
    let removed: Vec<&str> = fixed
        .definitions
        .keys()
        .filter(|name| !live.definitions.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !removed.is_empty() {
        sections.push(listing(REMOVED_HEADING, removed));
    }
    let added: Vec<String> = live
        .definitions
        .iter()
        .filter(|(name, _)| !fixed.definitions.contains_key(*name))
        .map(|(name, definition)| match summary(definition) {
            "" => name.clone(),
            summary => format!("{name}: {summary}"),
        })
        .collect();
    if !added.is_empty() {
        sections.push(listing(ADDED_HEADING, added));
    }
    let changed: Vec<String> = fixed
        .definitions
        .iter()
        .filter_map(|(name, definition)| {
            let live = live
                .definitions
                .get(name)
                .filter(|live| *live != definition)?;
            Some(definition_change(name, definition, live))
        })
        .collect();
    if !changed.is_empty() {
        sections.push(format!("{CHANGED_HEADING}\n\n{}", changed.join("\n\n")));
    }
    if fixed.system != live.system {
        sections.push(format!(
            "{SYSTEM_HEADING}\n\n{}",
            diff(&fixed.system, &live.system, SYSTEM_DISPLAY_PATH)
        ));
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

/// A description as a diff, which is how a skill added since shows; anything
/// else as the current definition, without the description already shown.
fn definition_change(name: &str, fixed: &Value, live: &Value) -> String {
    let mut text = format!("`{name}`");
    let description = |definition: &Value| {
        definition[DESCRIPTION_KEY]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let (before, after) = (description(fixed), description(live));
    if before != after {
        let _ = write!(text, " description:\n{}", diff(&before, &after, name));
    }
    let without_description = |definition: &Value| {
        let mut definition = definition.clone();
        if let Some(object) = definition.as_object_mut() {
            object.remove(DESCRIPTION_KEY);
        }
        definition
    };
    let rest = without_description(live);
    if without_description(fixed) != rest {
        let _ = write!(text, "\n{name} is now defined as: {rest}");
    }
    text
}

fn diff(before: &str, after: &str, display_path: &str) -> String {
    crate::diff::unified_text(
        before,
        after,
        &crate::diff::stat(before, after),
        display_path,
    )
}

fn listing(heading: &str, rows: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut text = heading.to_owned();
    for row in rows {
        let _ = write!(text, "\n- {}", row.as_ref());
    }
    text
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use caudra_providers::Message;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        ADDED_HEADING, CHANGED_HEADING, REMOVED_HEADING, SYSTEM_HEADING, Surface, SurfaceBaseline,
    };
    use crate::prompt::{CONFIGURATION_CHANGED_MARKER, CONFIGURATION_RESTORED_PROMPT};
    use crate::tools::DeferredTool;

    const KEY: &str = "anthropic/model";
    const OTHER_KEY: &str = "openai/model";
    const EPOCH: u64 = 3;
    const BRIEF: &str = "Be brief.";
    const THOROUGH: &str = "Be thorough.";
    const SYSTEM: &str = "You are Caudra.\nBe brief.\n";
    const EDITED_SYSTEM: &str = "You are Caudra.\nBe thorough.\n";
    const KEPT_DESCRIPTION: &str = "Read.";
    const EDITED_DESCRIPTION: &str = "Read files.";
    const KEPT: &str = "file_read";
    const DROPPED: &str = "file_write";
    const ADDED: &str = "image_generate";
    const ADDED_SUMMARY: &str = "Generate an image.";
    const DEFERRED: &str = "workflow";
    const PATH_PARAMETER: &str = "path";
    const TTL: Duration = Duration::from_secs(300);

    fn tool(name: &str, description: &str) -> Value {
        json!({"name": name, "description": description, "input_schema": {"type": "object"}})
    }

    fn surface(system: &str, tools: &[Value]) -> Surface {
        Surface::new(system.to_owned(), Value::from(tools.to_vec()), Vec::new())
    }

    fn base() -> Surface {
        surface(
            SYSTEM,
            &[tool(KEPT, KEPT_DESCRIPTION), tool(DROPPED, "Write.")],
        )
    }

    fn fixed_baseline() -> SurfaceBaseline<&'static str> {
        let mut baseline = SurfaceBaseline::default();
        baseline.settle(base(), true, KEY, EPOCH, &[]);
        baseline
    }

    fn system_of(baseline: &mut SurfaceBaseline<&'static str>, live: Surface) -> String {
        baseline
            .settle(live, false, KEY, EPOCH, &[])
            .surface
            .system
            .clone()
    }

    #[test]
    fn a_first_settle_fixes_the_live_prefix_without_a_notice() {
        let mut baseline = SurfaceBaseline::default();
        let settled = baseline.settle(base(), true, KEY, EPOCH, &[]);
        assert_eq!(settled.surface.system, SYSTEM);
        assert!(settled.notice.is_none());
        assert!(settled.retired.is_empty());
    }

    #[test]
    fn an_unchanged_configuration_keeps_the_prefix_quietly() {
        let mut baseline = fixed_baseline();
        let settled = baseline.settle(base(), false, KEY, EPOCH, &[]);
        assert!(settled.notice.is_none());
    }

    #[test]
    fn a_changed_configuration_keeps_the_prefix_and_describes_the_difference() {
        let mut baseline = fixed_baseline();
        let live = surface(
            EDITED_SYSTEM,
            &[tool(KEPT, EDITED_DESCRIPTION), tool(ADDED, ADDED_SUMMARY)],
        );
        let settled = baseline.settle(live, false, KEY, EPOCH, &[]);
        assert_eq!(settled.surface.system, SYSTEM);
        assert_eq!(settled.retired, [DROPPED]);
        let notice = settled.notice.unwrap();
        assert!(notice.contains(CONFIGURATION_CHANGED_MARKER));
        assert!(notice.contains(&format!("{REMOVED_HEADING}\n- {DROPPED}")));
        assert!(notice.contains(&format!("{ADDED_HEADING}\n- {ADDED}: {ADDED_SUMMARY}")));
        assert!(notice.contains(&format!("{CHANGED_HEADING}\n\n`{KEPT}` description:")));
        assert!(notice.contains(&format!("- {KEPT_DESCRIPTION}\n+ {EDITED_DESCRIPTION}")));
        assert!(!notice.contains(&format!("{KEPT} is now defined as")));
        assert!(notice.contains(SYSTEM_HEADING));
        assert!(notice.contains(&format!("- {BRIEF}\n+ {THOROUGH}")));
    }

    #[test]
    fn a_changed_schema_shows_the_current_definition() {
        let mut baseline = fixed_baseline();
        let mut reshaped = tool(KEPT, KEPT_DESCRIPTION);
        reshaped["input_schema"]["required"] = json!([PATH_PARAMETER]);
        let live = surface(SYSTEM, &[reshaped, tool(DROPPED, "Write.")]);
        let notice = baseline
            .settle(live, false, KEY, EPOCH, &[])
            .notice
            .unwrap();
        assert!(notice.contains(&format!("{KEPT} is now defined as: ")));
        assert!(notice.contains(PATH_PARAMETER));
        assert!(!notice.contains(&format!("`{KEPT}` description:")));
    }

    #[test]
    fn a_deferred_tool_counts_as_defined() {
        let mut baseline = fixed_baseline();
        let deferred = DeferredTool::new(DEFERRED, None, tool(DEFERRED, "Run workflows."));
        let live = Surface::new(
            SYSTEM.to_owned(),
            Value::from(vec![tool(KEPT, KEPT_DESCRIPTION), tool(DROPPED, "Write.")]),
            vec![deferred],
        );
        let notice = baseline
            .settle(live, false, KEY, EPOCH, &[])
            .notice
            .unwrap();
        assert!(notice.contains(&format!("{ADDED_HEADING}\n- {DEFERRED}")));
    }

    #[test]
    fn the_same_difference_renders_the_same_block() {
        let mut baseline = fixed_baseline();
        let edited = || surface(EDITED_SYSTEM, &[tool(KEPT, KEPT_DESCRIPTION)]);
        let first = baseline.settle(edited(), false, KEY, EPOCH, &[]).notice;
        let second = baseline.settle(edited(), false, KEY, EPOCH, &[]).notice;
        assert!(first.is_some());
        assert_eq!(first, second);
        assert_eq!(system_of(&mut baseline, edited()), SYSTEM);
    }

    #[test_case(false ; "configuration_put_back")]
    #[test_case(true ; "prefix_rebuilt")]
    fn a_difference_in_force_is_withdrawn_once(rebuild: bool) {
        let mut baseline = fixed_baseline();
        let notice = baseline
            .settle(surface(EDITED_SYSTEM, &[]), false, KEY, EPOCH, &[])
            .notice
            .unwrap();
        let mut history = vec![Message::observation(notice)];
        let live = if rebuild {
            surface(EDITED_SYSTEM, &[])
        } else {
            base()
        };
        let withdrawal = baseline.settle(live, rebuild, KEY, EPOCH, &history).notice;
        assert_eq!(withdrawal.as_deref(), Some(CONFIGURATION_RESTORED_PROMPT));
        history.push(Message::observation(withdrawal.unwrap()));
        let live = if rebuild {
            surface(EDITED_SYSTEM, &[])
        } else {
            base()
        };
        assert!(
            baseline
                .settle(live, false, KEY, EPOCH, &history)
                .notice
                .is_none()
        );
    }

    #[test]
    fn a_rebuild_adopts_the_live_prefix() {
        let mut baseline = fixed_baseline();
        let settled = baseline.settle(surface(EDITED_SYSTEM, &[]), true, OTHER_KEY, EPOCH, &[]);
        assert_eq!(settled.surface.system, EDITED_SYSTEM);
        assert!(settled.notice.is_none());
        assert!(!baseline.stale(&OTHER_KEY, EPOCH, None, Instant::now()));
    }

    #[test_case(OTHER_KEY, EPOCH, None, Duration::ZERO => true ; "key_moved")]
    #[test_case(KEY, EPOCH + 1, None, Duration::ZERO => true ; "conversation_replaced")]
    #[test_case(KEY, EPOCH, Some(TTL), TTL => true ; "cache_expired")]
    #[test_case(KEY, EPOCH, Some(TTL), TTL - Duration::from_secs(1) => false ; "cache_warm")]
    #[test_case(KEY, EPOCH, None, TTL * 100 => false ; "no_known_expiry")]
    fn staleness(key: &'static str, epoch: u64, ttl: Option<Duration>, idle: Duration) -> bool {
        let mut baseline = fixed_baseline();
        let used = Instant::now();
        baseline.touch(used);
        baseline.stale(&key, epoch, ttl, used + idle)
    }

    #[test]
    fn nothing_fixed_is_stale() {
        let baseline: SurfaceBaseline<&str> = SurfaceBaseline::default();
        assert!(baseline.stale(&KEY, EPOCH, None, Instant::now()));
    }
}
