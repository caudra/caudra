use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use caudra_models::Snapshot;
use jiff::Timestamp;

use crate::decision_pricing::DecisionModelEntry;
use crate::manifest::ProviderManifest;

use super::{bundled, convert};

static CURRENT: LazyLock<RwLock<&'static Current>> = LazyLock::new(|| {
    RwLock::new(Box::leak(Box::new(Current::from_snapshot(
        bundled::snapshot(),
    ))))
});

pub struct Current {
    pub revision: u64,
    pub snapshot_updated_at: Timestamp,
    pub content_sha256: String,
    pub manifests: &'static [ProviderManifest],
    pub decision_models: &'static [DecisionModelEntry],
    pub(crate) unsupported_providers: HashMap<String, &'static str>,
}

impl Current {
    pub(crate) fn from_snapshot(snapshot: Snapshot) -> Self {
        let (manifests, unsupported_providers) = convert::manifests(snapshot.payload.providers);
        Self {
            revision: 0,
            snapshot_updated_at: snapshot.updated_at,
            content_sha256: snapshot.content_sha256,
            manifests,
            decision_models: convert::decisions(snapshot.payload.decision_models),
            unsupported_providers,
        }
    }
}

pub fn current() -> &'static Current {
    *CURRENT
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[allow(
    dead_code,
    reason = "snapshot installation is wired to refresh in a later unit"
)]
pub(crate) fn install(mut next: Current) {
    let mut current = CURRENT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    next.revision = current.revision + 1;
    *current = Box::leak(Box::new(next));
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::DecisionProtocol;
    use caudra_models::{self as wire, Payload, Snapshot};
    use caudra_storage::StateDir;
    use test_case::test_case;

    use super::{Current, current};
    use crate::decision_pricing::{self, DECISION_MODELS};
    use crate::manifest::{BUILTINS, ManifestRegistry};
    use crate::model::{Model, ModelError, ModelPurpose, TokenUsage, lookup_entry};
    use crate::model_data::{bundled, install};
    use crate::providers::catalog::seed_catalog_for_tests;

    const NOW: &str = "2026-10-29T12:00:00Z";
    const LATER: &str = "2026-10-30T12:00:00Z";
    const PROVIDER: &str = "snapshot-test";
    const DROPPED_PROVIDER: &str = "future-provider";
    const BROAD: &str = "model";
    const NARROW: &str = "model-next";
    const FUTURE: &str = "future";
    const FAMILY_REASON: &str = "family \"future\" needs a newer Caudra";
    const REQUIREMENT_REASON: &str = "requirement \"future\" needs a newer Caudra";
    const UPGRADE_MESSAGE: &str = "Upgrade Caudra to use this model.";
    const UPDATED_NAME: &str = "Updated provider";
    const UPDATED_PRICE: f64 = 7.0;
    const MILLION: u32 = 1_000_000;
    const CATALOG: &str = r#"{
        "future-provider": {
            "name": "Future provider", "npm": "@ai-sdk/openai-compatible",
            "models": {"model": {"limit": {"context": 32000, "output": 4096}}}
        }
    }"#;
    const FIXTURE: &str = r#"{
        "providers": [{
            "slug": "snapshot-test", "display_name": "Snapshot test", "family": "generic",
            "fallback_context_window": 32000,
            "models": [{
                "prefixes": ["model"], "default": true, "family": "generic",
                "context_window": 32000, "window_excludes_output": true,
                "pricing": {"input": 1.0, "output": 2.0, "cache_write": 0.0, "cache_read": 0.0}
            }, {
                "prefixes": ["model-next"], "default": true, "small": true, "family": "generic",
                "context_window": 64000,
                "pricing": {"input": 3.0, "output": 4.0, "cache_write": 0.0, "cache_read": 0.0}
            }]
        }],
        "decision_models": [{
            "protocol": "openai", "prefixes": ["decision"], "source": "fixture",
            "pricing": {"input": 1.0, "output": 0.0, "cache_write": 0.0, "cache_read": 0.0}
        }]
    }"#;

    fn snapshot() -> Snapshot {
        Snapshot::stamp(
            serde_json::from_str(FIXTURE).unwrap(),
            None,
            NOW.parse().unwrap(),
        )
    }

    fn converted(payload: Payload) -> Current {
        let snapshot = Snapshot::stamp(payload, None, NOW.parse().unwrap());
        let snapshot = wire::parse_bounded(&wire::to_pretty_bytes(&snapshot)).unwrap();
        wire::validate(&snapshot).unwrap();
        Current::from_snapshot(snapshot)
    }

    #[test]
    fn converted_fixture_equals_every_const_table_field() {
        let snapshot = bundled::snapshot();
        wire::validate(&snapshot).unwrap();
        assert!(snapshot.hash_matches());
        let converted = Current::from_snapshot(snapshot);
        assert_eq!(converted.manifests.len(), BUILTINS.len());
        for (actual, expected) in converted.manifests.iter().zip(BUILTINS) {
            assert_eq!(actual, expected, "{}", expected.slug);
        }
        assert_eq!(converted.decision_models, DECISION_MODELS);
        assert_eq!(current().manifests, converted.manifests);
        assert_eq!(current().decision_models, converted.decision_models);
    }

    #[test_case(false, FAMILY_REASON; "unknown_family")]
    #[test_case(true, REQUIREMENT_REASON; "unknown_requirement")]
    fn unsupported_entry_is_hidden_refused_and_blocks_prefix_fallback(
        requirement: bool,
        reason: &str,
    ) {
        let mut payload = snapshot().payload;
        let entry = &mut payload.providers[0].models[1];
        if requirement {
            entry.requires.push(FUTURE.to_owned());
        } else {
            entry.family = wire::Family::Unknown(FUTURE.to_owned());
        }
        install(converted(payload));
        let manifest = ManifestRegistry::get(PROVIDER).unwrap();
        assert_eq!(
            manifest
                .listed_models_at(NOW.parse().unwrap())
                .map(|entry| entry.prefixes[0])
                .collect::<Vec<_>>(),
            [BROAD]
        );
        assert!(
            ManifestRegistry::default_from_manifest(
                manifest,
                ModelPurpose::Fast,
                NOW.parse().unwrap()
            )
            .is_none()
        );
        assert_eq!(
            ManifestRegistry::default_from_manifest(
                manifest,
                ModelPurpose::Best,
                NOW.parse().unwrap()
            )
            .unwrap()
            .prefixes,
            &[BROAD]
        );
        assert!(Model::curated_default(PROVIDER, ModelPurpose::Fast).is_none());
        for model_id in [NARROW.to_owned(), format!("{NARROW}-version")] {
            let entry = lookup_entry(manifest.models, &model_id).unwrap();
            assert_eq!(entry.prefixes, &[NARROW]);
            assert_eq!(entry.unsupported, Some(reason));
            let spec = format!("{PROVIDER}/{model_id}");
            let error = Model::from_spec(&spec).unwrap_err();
            assert!(error.to_string().contains(UPGRADE_MESSAGE));
            assert!(
                matches!(error, ModelError::NeedsNewerCaudra { model, reason: actual } if model == spec && actual == reason)
            );
        }
        let broad = Model::from_spec(&format!("{PROVIDER}/{BROAD}")).unwrap();
        assert!(broad.window_excludes_output);
    }

    #[test_case(false; "unknown_family")]
    #[test_case(true; "unknown_requirement")]
    fn unsupported_provider_is_dropped_without_losing_other_providers(requirement: bool) {
        let state = tempfile::tempdir().unwrap();
        seed_catalog_for_tests(
            serde_json::from_str(CATALOG).unwrap(),
            StateDir::from_path(state.path().to_owned()),
        );
        let spec = format!("{DROPPED_PROVIDER}/{BROAD}");
        assert!(Model::from_spec(&spec).is_ok());
        let mut payload = snapshot().payload;
        let mut provider = payload.providers[0].clone();
        provider.slug = DROPPED_PROVIDER.to_owned();
        if requirement {
            provider.requires.push(FUTURE.to_owned());
        } else {
            provider.family = wire::Family::Unknown(FUTURE.to_owned());
        }
        payload.providers.insert(0, provider);
        install(converted(payload));
        assert!(ManifestRegistry::get(DROPPED_PROVIDER).is_none());
        assert!(ManifestRegistry::for_slug(DROPPED_PROVIDER).is_none());
        let reason = if requirement {
            REQUIREMENT_REASON
        } else {
            FAMILY_REASON
        };
        assert!(
            matches!(Model::from_spec(&spec), Err(ModelError::NeedsNewerCaudra { model, reason: actual }) if model == spec && actual == reason)
        );
        assert_eq!(ManifestRegistry::builtins().len(), 1);
        assert!(Model::from_spec(&format!("{PROVIDER}/{BROAD}")).is_ok());
    }

    #[test_case(vec![], 2.0; "empty_windows")]
    #[test_case(vec![(24, 2)], 2.0; "invalid_start")]
    #[test_case(vec![(2, 25)], 2.0; "invalid_end")]
    #[test_case(vec![(2, 2)], 2.0; "empty_window")]
    #[test_case(vec![(1, 2)], 1.0; "flat_multiplier")]
    #[test_case(vec![(1, 2)], f64::INFINITY; "infinite_multiplier")]
    #[test_case(vec![(1, 2)], f64::NAN; "nan_multiplier")]
    fn unconvertible_schedule_drops_only_its_provider(windows: Vec<(u8, u8)>, multiplier: f64) {
        let mut snapshot = snapshot();
        let mut provider = snapshot.payload.providers[0].clone();
        provider.slug = DROPPED_PROVIDER.to_owned();
        provider.pricing_schedule = Some(wire::PricingSchedule {
            windows: windows
                .into_iter()
                .map(|(start, end)| wire::PricingWindow { start, end })
                .collect(),
            multiplier,
        });
        snapshot.payload.providers.insert(0, provider);
        let converted = Current::from_snapshot(snapshot);
        assert_eq!(converted.manifests.len(), 1);
        assert_eq!(converted.manifests[0].slug, PROVIDER);
    }

    #[test_case(&[]; "no_prefixes")]
    #[test_case(&[""]; "empty_prefix")]
    fn unconvertible_prefixes_cannot_reach_default_resolution(prefixes: &[&str]) {
        let mut snapshot = snapshot();
        snapshot.payload.providers[0].models[0].prefixes =
            prefixes.iter().map(|prefix| (*prefix).to_owned()).collect();
        install(Current::from_snapshot(snapshot));
        assert!(ManifestRegistry::get(PROVIDER).is_none());
        assert!(Model::curated_default(PROVIDER, ModelPurpose::Best).is_none());
    }

    #[test]
    fn install_bumps_revision_and_switches_all_registry_readers() {
        let previous = current();
        let mut snapshot = snapshot();
        snapshot.payload.providers[0].display_name = UPDATED_NAME.to_owned();
        snapshot.payload.decision_models[0].pricing.input = UPDATED_PRICE;
        snapshot = Snapshot::stamp(snapshot.payload, None, LATER.parse().unwrap());
        let expected_hash = snapshot.content_sha256.clone();
        install(Current::from_snapshot(snapshot));
        assert_eq!(current().revision, previous.revision + 1);
        assert_eq!(current().snapshot_updated_at, LATER.parse().unwrap());
        assert_eq!(current().content_sha256, expected_hash);
        assert_eq!(
            ManifestRegistry::get(PROVIDER).unwrap().display_name,
            UPDATED_NAME
        );
        assert_eq!(
            ManifestRegistry::for_slug(PROVIDER).unwrap().display_name,
            UPDATED_NAME
        );
        assert_eq!(ManifestRegistry::builtins()[0].display_name, UPDATED_NAME);
        assert_eq!(decision_pricing::entries()[0].pricing.input, UPDATED_PRICE);
        assert_eq!(
            decision_pricing::decision_cost(
                &DecisionProtocol::OpenAI,
                &["decision"],
                &TokenUsage {
                    input: MILLION,
                    ..TokenUsage::default()
                }
            ),
            Some(UPDATED_PRICE)
        );
        install(Current::from_snapshot(bundled::snapshot()));
        assert_eq!(current().revision, previous.revision + 2);
        assert_eq!(previous.manifests, BUILTINS);
    }
}
