use std::fmt;

use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use thiserror::Error;

/// Indexed by [`Feature`] discriminant, so the enum and its config keys stay one list.
const FEATURE_KEYS: [&str; 5] = [
    "workflows",
    "sandboxes",
    "remote_workcell",
    "lua_plugins",
    "decision_engine",
];

/// A first-party capability that stays off until the user opts in through the
/// `[experimental]` table of the global `caudra.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    Workflows,
    Sandboxes,
    RemoteWorkcell,
    LuaPlugins,
    DecisionEngine,
}

impl Feature {
    pub const ALL: [Self; 5] = [
        Self::Workflows,
        Self::Sandboxes,
        Self::RemoteWorkcell,
        Self::LuaPlugins,
        Self::DecisionEngine,
    ];

    pub fn key(self) -> &'static str {
        FEATURE_KEYS[self as usize]
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|feature| feature.key() == key)
    }

    /// What the switch covers, phrased to read before "are experimental".
    pub fn subject(self) -> &'static str {
        match self {
            Self::Workflows => "workflows",
            Self::Sandboxes => "managed sandboxes",
            Self::RemoteWorkcell => "direct remote Workcell connections",
            Self::LuaPlugins => "Lua plugins and init.lua",
            Self::DecisionEngine => "the decision engine and Auto mode",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "experimental.{}", self.key())
    }
}

/// The experimental switches one process resolved at startup. Every flag is
/// off unless the global `caudra.toml` turns it on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FeatureFlags(u8);

impl FeatureFlags {
    pub const NONE: Self = Self(0);

    pub fn all() -> Self {
        Feature::ALL.into_iter().fold(Self::NONE, Self::with)
    }

    #[must_use]
    pub const fn with(self, feature: Feature) -> Self {
        Self(self.0 | feature.bit())
    }

    #[must_use]
    pub const fn without(self, feature: Feature) -> Self {
        Self(self.0 & !feature.bit())
    }

    pub const fn enabled(self, feature: Feature) -> bool {
        self.0 & feature.bit() != 0
    }

    pub fn require(self, feature: Feature) -> Result<(), FeatureDisabled> {
        if self.enabled(feature) {
            Ok(())
        } else {
            Err(FeatureDisabled(feature))
        }
    }

    /// A saved workspace reconnects only through a source this process
    /// enabled. Managed sandboxes are known by their lifecycle record, since
    /// they reach Workcell over the same remote transport as a direct
    /// connection; any other non-local source is a direct one.
    pub fn require_source(
        self,
        binding: Option<&StoredWorkspaceBinding>,
    ) -> Result<(), FeatureDisabled> {
        match binding {
            Some(binding) if binding.sandbox_record().is_some() => self.require(Feature::Sandboxes),
            Some(binding) if !binding.is_local() => self.require(Feature::RemoteWorkcell),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "{subject} are experimental and turned off; set `{feature} = true` in the global caudra.toml and restart Caudra to use them",
    subject = .0.subject(),
    feature = .0
)]
pub struct FeatureDisabled(pub Feature);

impl<'de> Deserialize<'de> for FeatureFlags {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(FlagsVisitor)
    }
}

struct FlagsVisitor;

impl<'de> Visitor<'de> for FlagsVisitor {
    type Value = FeatureFlags;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a table of experimental feature switches")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<FeatureFlags, A::Error> {
        let mut flags = FeatureFlags::NONE;
        while let Some(key) = map.next_key::<String>()? {
            let feature = Feature::from_key(&key)
                .ok_or_else(|| de::Error::unknown_field(&key, &FEATURE_KEYS))?;
            if map.next_value::<bool>()? {
                flags = flags.with(feature);
            }
        }
        Ok(flags)
    }
}

#[cfg(test)]
mod tests {
    use super::{FEATURE_KEYS, Feature, FeatureDisabled, FeatureFlags};
    use test_case::test_case;

    #[test]
    fn keys_follow_discriminants() {
        for (index, feature) in Feature::ALL.into_iter().enumerate() {
            assert_eq!(feature as usize, index);
            assert_eq!(Feature::from_key(FEATURE_KEYS[index]), Some(feature));
        }
    }

    #[test]
    fn default_enables_nothing() {
        for feature in Feature::ALL {
            assert!(!FeatureFlags::default().enabled(feature));
            assert_eq!(
                FeatureFlags::NONE.require(feature),
                Err(FeatureDisabled(feature))
            );
        }
    }

    #[test]
    fn each_flag_is_independent() {
        for feature in Feature::ALL {
            let flags = FeatureFlags::NONE.with(feature);
            for other in Feature::ALL {
                assert_eq!(flags.enabled(other), other == feature);
            }
            assert!(!FeatureFlags::all().without(feature).enabled(feature));
        }
    }

    #[test_case(Feature::Workflows, "experimental.workflows"; "workflows")]
    #[test_case(Feature::DecisionEngine, "experimental.decision_engine"; "decision_engine")]
    fn disabled_error_names_the_key(feature: Feature, key: &str) {
        let message = FeatureDisabled(feature).to_string();
        assert!(message.contains(&format!("`{key} = true`")), "{message}");
        assert!(message.contains("caudra.toml"), "{message}");
    }
}
