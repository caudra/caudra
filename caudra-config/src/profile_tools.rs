use std::{collections::BTreeMap, fmt, marker::PhantomData};

use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, Visitor},
};

use crate::{INTERNAL_COMPANION_TOOL_NAMES, ToolKey, is_builtin_tool};

pub const TOOL_POLICY_GROUPS: &[(ToolPolicyGroup, &[&str])] = &[
    (ToolPolicyGroup::Web, &["websearch", "webfetch"]),
    (
        ToolPolicyGroup::Files,
        &[
            "file_read",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "file_glob",
            "file_grep",
            "file_index",
        ],
    ),
    (
        ToolPolicyGroup::CodeGraph,
        &[
            "code_map",
            "code_context",
            "code_refs",
            "code_impact",
            "code_expand",
        ],
    ),
    (
        ToolPolicyGroup::Execution,
        &["shell", "python_execution", "execution_environment"],
    ),
    (
        ToolPolicyGroup::Delegation,
        &["task", "task_control", "workflow"],
    ),
    (
        ToolPolicyGroup::Support,
        &["batch", "question", "todo_write", "plan"],
    ),
    (ToolPolicyGroup::Images, &["view_image", "image_generate"]),
    (
        ToolPolicyGroup::Messaging,
        &["list_sessions", "send_message"],
    ),
];
const RESERVED_SELECTORS: &[&str] = &["tool_search", "report_to_parent", "structured_output"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicyGroup {
    Web,
    Files,
    CodeGraph,
    Execution,
    Delegation,
    Support,
    Images,
    Messaging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileToolExposure {
    Eager,
    Lazy,
    Disabled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileToolDefault {
    #[default]
    Inherit,
    Eager,
    Lazy,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileToolSource {
    Native,
    Mcp,
    Custom,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileToolPolicy {
    pub default: ProfileToolDefault,
    #[serde(deserialize_with = "unique_map")]
    pub groups: BTreeMap<ToolPolicyGroup, ProfileToolExposure>,
    #[serde(deserialize_with = "selectors")]
    pub overrides: BTreeMap<String, ProfileToolExposure>,
}

impl ProfileToolPolicy {
    pub fn exposure(&self, name: &str, source: ProfileToolSource) -> Option<ProfileToolExposure> {
        if let Some(exposure) = self.overrides.get(name) {
            return Some(*exposure);
        }
        if source == ProfileToolSource::Mcp
            && let Some((server, _)) = name.split_once('.')
            && let Some(exposure) = self.overrides.get(&format!("{server}.*"))
        {
            return Some(*exposure);
        }
        if source == ProfileToolSource::Native
            && let Some((group, _)) = TOOL_POLICY_GROUPS
                .iter()
                .find(|(_, names)| names.contains(&name))
            && let Some(exposure) = self.groups.get(group)
        {
            return Some(*exposure);
        }
        match self.default {
            ProfileToolDefault::Inherit => None,
            ProfileToolDefault::Eager => Some(ProfileToolExposure::Eager),
            ProfileToolDefault::Lazy => Some(ProfileToolExposure::Lazy),
            ProfileToolDefault::Disabled => Some(ProfileToolExposure::Disabled),
        }
    }

    pub fn validate_bindings<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), String> {
        let names: Vec<_> = names.into_iter().collect();
        for name in self.overrides.keys() {
            if !name.contains('.') && !is_builtin_tool(name) && !names.contains(&name.as_str()) {
                return Err(format!(
                    "tools.overrides: unknown tool {name:?}; no registered binding"
                ));
            }
        }
        Ok(())
    }

    pub fn is_legacy(&self) -> bool {
        self.default == ProfileToolDefault::Inherit
            && self.groups.is_empty()
            && self.overrides.is_empty()
    }
}

fn selectors<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, ProfileToolExposure>, D::Error> {
    let selectors: BTreeMap<String, ProfileToolExposure> = unique_map(deserializer)?;
    for name in selectors.keys() {
        if RESERVED_SELECTORS.contains(&name.as_str())
            || INTERNAL_COMPANION_TOOL_NAMES.contains(&name.as_str())
        {
            return Err(de::Error::custom(format!(
                "tools.overrides: {name:?} is host-owned infrastructure"
            )));
        }
        match ToolKey::parse(name).map_err(de::Error::custom)? {
            ToolKey::Wildcard => {
                return Err(de::Error::custom(
                    "tools.overrides: use default instead of '*'",
                ));
            }
            ToolKey::Native(_) if name.contains('*') => {
                return Err(de::Error::custom(
                    "tools.overrides: arbitrary globs are not supported",
                ));
            }
            _ => {}
        }
    }
    Ok(selectors)
}

fn unique_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord + fmt::Debug,
    V: Deserialize<'de>,
{
    struct UniqueMap<K, V>(PhantomData<(K, V)>);
    impl<'de, K: Deserialize<'de> + Ord + fmt::Debug, V: Deserialize<'de>> Visitor<'de>
        for UniqueMap<K, V>
    {
        type Value = BTreeMap<K, V>;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map without duplicate keys")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut entries = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<K, V>()? {
                if entries.contains_key(&key) {
                    return Err(de::Error::custom(format!(
                        "duplicate tool policy key {key:?}"
                    )));
                }
                entries.insert(key, value);
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_map(UniqueMap(PhantomData))
}

#[cfg(test)]
mod tests {
    use super::{ProfileToolExposure, ProfileToolPolicy, ProfileToolSource, TOOL_POLICY_GROUPS};
    use std::collections::HashSet;
    use test_case::test_case;

    #[test_case("{}", true; "empty")]
    #[test_case(r#"{"default":"inherit"}"#, true; "inherit")]
    #[test_case(r#"{"default":"lazy","groups":{"support":"eager"}}"#, true; "states")]
    #[test_case(r#"{"extra":true}"#, false; "unknown_field")]
    #[test_case(r#"{"groups":{"unknown":"eager"}}"#, false; "unknown_group")]
    #[test_case(r#"{"groups":{"documents":"eager"}}"#, false; "removed_document_group")]
    #[test_case(r#"{"groups":{"web":"inherit"}}"#, false; "entry_inherit")]
    #[test_case(r#"{"overrides":{"web*":"lazy"}}"#, false; "glob")]
    #[test_case(r#"{"overrides":{"a..b":"lazy"}}"#, false; "malformed_mcp")]
    #[test_case(r#"{"overrides":{"tool_search":"lazy"}}"#, false; "loader")]
    #[test_case(r#"{"overrides":{"tool_output":"disabled"}}"#, false; "pager")]
    #[test_case(r#"{"overrides":{"file_read":"lazy","file_read":"eager"}}"#, false; "duplicate_selector")]
    #[test_case(r#"{"groups":{"web":"lazy","web":"eager"}}"#, false; "duplicate_group")]
    fn strict_policy(json: &str, valid: bool) {
        assert_eq!(
            serde_json::from_str::<ProfileToolPolicy>(json).is_ok(),
            valid
        );
    }

    #[test_case("file_read", ProfileToolSource::Native, ProfileToolExposure::Eager; "exact")]
    #[test_case("file_write", ProfileToolSource::Native, ProfileToolExposure::Lazy; "group")]
    #[test_case("file_write", ProfileToolSource::Custom, ProfileToolExposure::Disabled; "shadow")]
    #[test_case("github.read", ProfileToolSource::Mcp, ProfileToolExposure::Lazy; "server")]
    #[test_case("github.delete", ProfileToolSource::Mcp, ProfileToolExposure::Disabled; "mcp_exact")]
    fn resolution(name: &str, source: ProfileToolSource, expected: ProfileToolExposure) {
        let policy: ProfileToolPolicy = serde_json::from_str(r#"{"default":"disabled","groups":{"files":"lazy"},"overrides":{"file_read":"eager","github.*":"lazy","github.delete":"disabled"}}"#).unwrap();
        assert_eq!(policy.exposure(name, source), Some(expected));
    }

    #[test]
    fn disjoint_builtin_groups() {
        let mut seen = HashSet::new();
        for (_, names) in TOOL_POLICY_GROUPS {
            for name in *names {
                assert!(crate::is_builtin_tool(name));
                assert!(seen.insert(name));
            }
        }
    }

    #[test_case("local_document_read")]
    #[test_case("local_document_write")]
    #[test_case("local_document_apply_patch")]
    fn removed_tools_have_no_builtin_binding(name: &str) {
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "overrides": {name: "eager"}
        }))
        .unwrap();
        assert!(!crate::is_builtin_tool(name));
        assert!(policy.validate_bindings([]).is_err());
    }
}
