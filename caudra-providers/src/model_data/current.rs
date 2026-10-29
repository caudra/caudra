use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use caudra_models::Snapshot;
use jiff::Timestamp;

use crate::decision_pricing::DecisionModelEntry;
use crate::manifest::ProviderManifest;
use crate::providers::catalog::CatalogData;

use super::{bundled, convert};

static CURRENT: OnceLock<RwLock<Arc<Current>>> = OnceLock::new();

fn registry() -> &'static RwLock<Arc<Current>> {
    CURRENT.get_or_init(|| RwLock::new(Arc::new(Current::from_snapshot(initial_snapshot()))))
}

/// Selects the bundled data before any lookup, for reproducible maintainer tools.
pub fn initialize_bundled() {
    CURRENT.get_or_init(|| RwLock::new(Arc::new(Current::from_snapshot(bundled::snapshot()))));
}

fn initial_snapshot() -> Snapshot {
    let bundled = bundled::snapshot();
    #[cfg(not(test))]
    return super::refresh::startup_snapshot(bundled);
    #[cfg(test)]
    bundled
}

pub struct Current {
    pub revision: u64,
    pub snapshot_updated_at: Timestamp,
    pub content_sha256: String,
    pub manifests: &'static [ProviderManifest],
    pub decision_models: &'static [DecisionModelEntry],
    pub(crate) catalog: CatalogData,
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
            catalog: convert::catalog(snapshot.payload.catalog),
            unsupported_providers,
        }
    }
}

pub fn current() -> Arc<Current> {
    registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
pub(crate) fn install(snapshot: Snapshot) {
    let mut current = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if snapshot.content_sha256 == current.content_sha256 {
        return;
    }
    let mut next = Current::from_snapshot(snapshot);
    next.revision = current.revision + 1;
    *current = Arc::new(next);
}

pub(super) fn install_newer(snapshot: Snapshot) -> bool {
    let mut current = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if snapshot.updated_at <= current.snapshot_updated_at
        || snapshot.content_sha256 == current.content_sha256
    {
        return false;
    }
    let mut next = Current::from_snapshot(snapshot);
    next.revision = current.revision + 1;
    *current = Arc::new(next);
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_config::decisions::DecisionProtocol;
    use caudra_models::{self as wire, Payload, Snapshot};
    use test_case::test_case;

    use super::{Current, current};
    use crate::decision_pricing;
    use crate::manifest::ManifestRegistry;
    use crate::model::{Model, ModelError, ModelPurpose, TokenUsage, lookup_entry};
    use crate::model_data::{bundled, install};
    use crate::providers::catalog::{catalog_provider, model_ids, model_meta};

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
    const UPDATED_CONTEXT: u32 = 96_000;
    const CATALOG_PROVIDER: &str = "catalog-test";
    const CATALOG_MODEL: &str = "catalog-model";
    const CATALOG_KEY_ENV: &str = "CAUDRA_SNAPSHOT_TEST_KEY";
    const CATALOG_API: &str = "https://snapshot.example/v1";
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

    fn validated(payload: Payload) -> Snapshot {
        let snapshot = Snapshot::stamp(payload, None, NOW.parse().unwrap());
        let snapshot = wire::parse_bounded(&wire::to_pretty_bytes(&snapshot)).unwrap();
        wire::validate(&snapshot).unwrap();
        snapshot
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
        install(validated(payload));
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
        let mut payload = snapshot().payload;
        payload.catalog = serde_json::from_str(CATALOG).unwrap();
        install(validated(payload.clone()));
        let spec = format!("{DROPPED_PROVIDER}/{BROAD}");
        assert!(Model::from_spec(&spec).is_ok());
        let mut provider = payload.providers[0].clone();
        provider.slug = DROPPED_PROVIDER.to_owned();
        if requirement {
            provider.requires.push(FUTURE.to_owned());
        } else {
            provider.family = wire::Family::Unknown(FUTURE.to_owned());
        }
        payload.providers.insert(0, provider);
        install(validated(payload));
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
        install(Snapshot::stamp(
            snapshot.payload,
            None,
            NOW.parse().unwrap(),
        ));
        assert!(ManifestRegistry::get(PROVIDER).is_none());
        assert!(Model::curated_default(PROVIDER, ModelPurpose::Best).is_none());
    }

    #[test]
    fn install_bumps_revision_and_switches_all_registry_readers() {
        let previous = current();
        let mut snapshot = snapshot();
        snapshot.payload.providers[0].display_name = UPDATED_NAME.to_owned();
        snapshot.payload.decision_models[0].pricing.input = UPDATED_PRICE;
        snapshot.payload.catalog.insert(
            CATALOG_PROVIDER.to_owned(),
            wire::CatalogProvider {
                name: UPDATED_NAME.to_owned(),
                env: vec![CATALOG_KEY_ENV.to_owned()],
                npm: "@ai-sdk/openai-compatible".to_owned(),
                api: Some(CATALOG_API.to_owned()),
                models: [(
                    CATALOG_MODEL.to_owned(),
                    wire::CatalogModel {
                        limit: Some(wire::CatalogLimits {
                            context: Some(UPDATED_CONTEXT),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )]
                .into(),
            },
        );
        snapshot = Snapshot::stamp(snapshot.payload, None, LATER.parse().unwrap());
        let expected_hash = snapshot.content_sha256.clone();
        install(snapshot);
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
        let catalog = catalog_provider(CATALOG_PROVIDER).unwrap();
        assert_eq!(catalog.display_name, UPDATED_NAME);
        assert_eq!(catalog.env_keys, [CATALOG_KEY_ENV]);
        assert_eq!(catalog.base_url.as_deref(), Some(CATALOG_API));
        assert_eq!(model_ids(CATALOG_PROVIDER), [CATALOG_MODEL]);
        assert_eq!(
            model_meta(CATALOG_PROVIDER, CATALOG_MODEL).unwrap().context,
            UPDATED_CONTEXT
        );
        assert_eq!(
            Model::from_spec(&format!("{CATALOG_PROVIDER}/{CATALOG_MODEL}"))
                .unwrap()
                .context_window,
            UPDATED_CONTEXT
        );
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
        install(bundled::snapshot());
        assert_eq!(current().revision, previous.revision + 2);
        assert_eq!(previous.manifests, current().manifests);
        assert!(catalog_provider(CATALOG_PROVIDER).is_none());
        assert!(model_meta(CATALOG_PROVIDER, CATALOG_MODEL).is_none());
    }

    #[test_case(NOW; "same_timestamp")]
    #[test_case(LATER; "newer_timestamp_same_content")]
    fn identical_content_keeps_the_existing_snapshot(at: &str) {
        let snapshot = snapshot();
        install(snapshot.clone());
        let previous = current();
        install(Snapshot::stamp(snapshot.payload, None, at.parse().unwrap()));
        assert!(Arc::ptr_eq(&previous, &current()));
        assert_eq!(current().revision, previous.revision);
        assert_eq!(current().snapshot_updated_at, NOW.parse().unwrap());
    }

    #[test]
    fn install_releases_the_previous_catalog_after_its_last_reader() {
        let previous = current();
        let weak = Arc::downgrade(&previous);
        install(snapshot());
        assert!(weak.upgrade().is_some());
        drop(previous);
        assert!(weak.upgrade().is_none());
    }
}
