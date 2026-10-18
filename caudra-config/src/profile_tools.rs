use std::{collections::BTreeMap, fmt, marker::PhantomData};

use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, Visitor},
};

use crate::{
    CODE_REQUIRED, CODE_TOOL_NAME, INTERNAL_COMPANION_TOOL_NAMES, ToolKey, is_builtin_tool,
};

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
        ToolPolicyGroup::SourceCode,
        &["source_code_context", "source_code_refs"],
    ),
    (
        ToolPolicyGroup::Execution,
        &["shell", "execution_environment"],
    ),
    (
        ToolPolicyGroup::Delegation,
        &["task", "task_control", "workflow", "automation"],
    ),
    (
        ToolPolicyGroup::Support,
        &["batch", "question", "todo_write", "plan"],
    ),
    (ToolPolicyGroup::Images, &["view_image", "image_generate"]),
    (
        ToolPolicyGroup::Messaging,
        &[
            "list_sessions",
            "send_message",
            "publish_message",
            "read_topic",
            "work_assignment",
        ],
    ),
];
const RESERVED_SELECTORS: &[&str] = &["report_to_parent", "structured_output"];
pub const LAZY_REMOVED: &str = "`lazy` was removed with `tool_search`; use `eager` or `disabled`";
const INHERIT: &str = "inherit";
const EAGER: &str = "eager";
const DISABLED: &str = "disabled";
const LAZY: &str = "lazy";
const DEFAULT_STATES: &[&str] = &[INHERIT, EAGER, DISABLED];
const EXPOSURE_STATES: &[&str] = &[EAGER, DISABLED];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicyGroup {
    Web,
    Files,
    SourceCode,
    Execution,
    Delegation,
    Support,
    Images,
    Messaging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileToolExposure {
    Eager,
    Disabled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProfileToolDefault {
    #[default]
    Inherit,
    Eager,
    Disabled,
}

impl<'de> Deserialize<'de> for ProfileToolDefault {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = String::deserialize(deserializer)?;
        match state.as_str() {
            INHERIT => Ok(Self::Inherit),
            EAGER => Ok(Self::Eager),
            DISABLED => Ok(Self::Disabled),
            LAZY => Err(de::Error::custom(LAZY_REMOVED)),
            other => Err(de::Error::unknown_variant(other, DEFAULT_STATES)),
        }
    }
}

impl<'de> Deserialize<'de> for ProfileToolExposure {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match ProfileToolDefault::deserialize(deserializer)? {
            ProfileToolDefault::Inherit => {
                Err(de::Error::unknown_variant(INHERIT, EXPOSURE_STATES))
            }
            ProfileToolDefault::Eager => Ok(Self::Eager),
            ProfileToolDefault::Disabled => Ok(Self::Disabled),
        }
    }
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
    for (name, exposure) in &selectors {
        if name == CODE_TOOL_NAME && *exposure == ProfileToolExposure::Disabled {
            return Err(de::Error::custom(CODE_REQUIRED));
        }
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
    use super::{
        LAZY_REMOVED, ProfileToolExposure, ProfileToolPolicy, ProfileToolSource, TOOL_POLICY_GROUPS,
    };
    use crate::CODE_REQUIRED;
    use std::collections::HashSet;
    use test_case::test_case;

    #[test_case("{}", true; "empty")]
    #[test_case(r#"{"default":"inherit"}"#, true; "inherit")]
    #[test_case(r#"{"default":"disabled","groups":{"support":"eager"}}"#, true; "states")]
    #[test_case(r#"{"default":"disabled"}"#, true; "default_disabled_keeps_code")]
    #[test_case(r#"{"overrides":{"code":"eager"}}"#, true; "code_eager")]
    #[test_case(r#"{"extra":true}"#, false; "unknown_field")]
    #[test_case(r#"{"groups":{"unknown":"eager"}}"#, false; "unknown_group")]
    #[test_case(r#"{"groups":{"documents":"eager"}}"#, false; "removed_document_group")]
    #[test_case(r#"{"groups":{"source_code":"eager"}}"#, true; "source_code_group")]
    #[test_case(r#"{"groups":{"code_graph":"eager"}}"#, false; "renamed_code_graph_group")]
    #[test_case(r#"{"groups":{"web":"inherit"}}"#, false; "entry_inherit")]
    #[test_case(r#"{"overrides":{"web*":"eager"}}"#, false; "glob")]
    #[test_case(r#"{"overrides":{"a..b":"eager"}}"#, false; "malformed_mcp")]
    #[test_case(r#"{"overrides":{"tool_output":"disabled"}}"#, false; "pager")]
    #[test_case(r#"{"overrides":{"file_read":"disabled","file_read":"eager"}}"#, false; "duplicate_selector")]
    #[test_case(r#"{"groups":{"web":"disabled","web":"eager"}}"#, false; "duplicate_group")]
    fn strict_policy(json: &str, valid: bool) {
        assert_eq!(
            serde_json::from_str::<ProfileToolPolicy>(json).is_ok(),
            valid
        );
    }

    #[test_case(r#"{"default":"lazy"}"#, LAZY_REMOVED; "lazy_default")]
    #[test_case(r#"{"groups":{"web":"lazy"}}"#, LAZY_REMOVED; "lazy_group")]
    #[test_case(r#"{"overrides":{"github.*":"lazy"}}"#, LAZY_REMOVED; "lazy_override")]
    #[test_case(r#"{"overrides":{"code":"disabled"}}"#, CODE_REQUIRED; "code_disabled")]
    fn removed_or_required_states_name_why(json: &str, expected: &str) {
        let error = serde_json::from_str::<ProfileToolPolicy>(json).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test_case("file_read", ProfileToolSource::Native, ProfileToolExposure::Eager; "exact")]
    #[test_case("file_write", ProfileToolSource::Native, ProfileToolExposure::Eager; "group")]
    #[test_case("file_write", ProfileToolSource::Custom, ProfileToolExposure::Disabled; "shadow")]
    #[test_case("github.read", ProfileToolSource::Mcp, ProfileToolExposure::Eager; "server")]
    #[test_case("github.delete", ProfileToolSource::Mcp, ProfileToolExposure::Disabled; "mcp_exact")]
    fn resolution(name: &str, source: ProfileToolSource, expected: ProfileToolExposure) {
        let policy: ProfileToolPolicy = serde_json::from_str(r#"{"default":"disabled","groups":{"files":"eager"},"overrides":{"file_read":"eager","github.*":"eager","github.delete":"disabled"}}"#).unwrap();
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
    #[test_case("code_context")]
    #[test_case("code_refs")]
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
