//! Tools a `code` snippet reaches through a module instead of the model calling them: they stay
//! registered, so permissions, records, the ledger and their cards work as they always did, but
//! dispatch runs one only for the module call that names it.

use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use serde_json::Value;

const DETAIL_FIELD: &str = "detail";

/// Each of [`caudra_config::CODE_HOSTED_TOOLS`] with the module call that replaced calling it directly.
const HOSTED_CALLS: &[(&str, &str)] = &[
    ("image_generate", "await image.generate(prompt, out)"),
    ("execution_environment", "await host.environment()"),
    ("workflow", "await workflow.<action>(...)"),
    ("automation", "await automation.<action>(...)"),
];

/// One module call of a hosted tool, carried on the call's context: dispatch accepts the tool for
/// it alone, shows `called_as` in the permission prompt, and the tool leaves what the snippet
/// receives in `value` when it has more than its text.
#[derive(Clone, Debug)]
pub struct HostedCall {
    tool: &'static str,
    pub called_as: String,
    value: Arc<Mutex<Option<Value>>>,
}

impl HostedCall {
    pub fn new(tool: &'static str, called_as: String) -> Self {
        Self {
            tool,
            called_as,
            value: Arc::default(),
        }
    }

    pub fn is_for(&self, tool: &str) -> bool {
        self.tool == tool
    }

    pub fn offer(&self, value: Value) {
        *self.value.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
    }

    /// Offers a runtime's answer, tagged as `{"kind", "detail"}`, as its `detail`: the record
    /// itself, or `None` for an answer that carries none.
    pub fn offer_detail(&self, answer: &impl Serialize) {
        if let Ok(mut value) = serde_json::to_value(answer) {
            self.offer(value[DETAIL_FIELD].take());
        }
    }

    pub fn take(&self) -> Option<Value> {
        self.value
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// The module call a snippet makes in place of calling hosted `tool` directly.
pub fn module_call(tool: &str) -> Option<&'static str> {
    HOSTED_CALLS
        .iter()
        .find(|(hosted, _)| *hosted == tool)
        .map(|(_, call)| *call)
}

/// Why a direct call of `tool` does not run, when only a module call may run it.
pub fn direct_call_refusal(tool: &str) -> Option<String> {
    module_call(tool).map(|call| {
        format!("`{tool}` is called from the `code` tool: `{call}`. See `help()` in a snippet.")
    })
}

#[cfg(test)]
mod tests {
    use caudra_config::CODE_HOSTED_TOOLS;

    use super::*;

    #[test]
    fn every_hosted_tool_names_its_module_call() {
        for tool in CODE_HOSTED_TOOLS {
            assert!(direct_call_refusal(tool).is_some(), "{tool}");
        }
        assert_eq!(HOSTED_CALLS.len(), CODE_HOSTED_TOOLS.len());
    }
}
