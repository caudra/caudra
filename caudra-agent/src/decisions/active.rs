//! Joins the shared `[decisions]` settings with the endpoint the user made
//! active, read from `providers.toml` and the `decisions.active` state row.

use caudra_config::decisions::{
    DecisionApiKey, DecisionEndpoint, DecisionEndpointDef, DecisionsConfig,
};
use caudra_config::providers::{ProvidersConfig, ProvidersFile, ProvidersFileError};
use caudra_storage::StateDir;
use caudra_storage::auth::{
    delete_decision_credentials, load_decision_credentials, save_decision_credentials,
};
use caudra_storage::decisions::{persist_active_decision_endpoint, read_active_decision_endpoint};
use tracing::{info, warn};

use super::DecisionsError;

/// What [`save_decision_endpoint`] did, for the caller to report and, when
/// `active`, to rebuild the running engine from `endpoint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedDecisionEndpoint {
    pub endpoint: DecisionEndpoint,
    pub added: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedDecisionEndpoint {
    pub removed: bool,
    /// It was the active endpoint, so decisions are off now.
    pub deactivated: bool,
}

/// `name`'s endpoint from `providers.toml`, or `None` when it has no table.
/// A table that does not validate is an error, so a typo never reads as off.
pub fn resolve_decision_endpoint(
    providers: &ProvidersConfig,
    name: &str,
) -> Result<Option<DecisionEndpoint>, DecisionsError> {
    Ok(providers
        .decisions
        .get(name)
        .map(|def| def.resolve(name))
        .transpose()?)
}

/// `config` with the active endpoint injected. `providers` is only read when
/// an endpoint is active. An active name whose table is gone turns decisions
/// off with a warning naming it.
pub fn resolve_active_decisions(
    config: &DecisionsConfig,
    state_dir: &StateDir,
    providers: impl FnOnce() -> ProvidersConfig,
) -> Result<DecisionsConfig, DecisionsError> {
    let endpoint = match read_active_decision_endpoint(state_dir) {
        Some(name) => {
            let endpoint = resolve_decision_endpoint(&providers(), &name)?;
            if endpoint.is_none() {
                warn!(endpoint = %name, "active decision endpoint is not in providers.toml; decisions are off");
            }
            endpoint
        }
        None => None,
    };
    Ok(config.clone().with_endpoint(endpoint))
}

/// The saved key for `endpoint`, else the global value of its `api_key_env`.
pub fn endpoint_api_key(
    endpoint: &DecisionEndpoint,
    state_dir: &StateDir,
) -> Result<Option<DecisionApiKey>, DecisionsError> {
    let stored = load_decision_credentials(state_dir, &endpoint.name)?;
    Ok(endpoint.api_key(stored.map(|credentials| credentials.api_key))?)
}

/// Saves `name` as `def` in `providers.toml`, and `api_key` when it is not
/// blank; a blank key keeps the stored one. A new endpoint becomes active.
/// Re-keying an existing one keeps whichever endpoint was active.
pub fn save_decision_endpoint(
    state_dir: &StateDir,
    name: &str,
    def: DecisionEndpointDef,
    api_key: Option<&str>,
) -> Result<SavedDecisionEndpoint, DecisionsError> {
    save_endpoint_in(
        &mut ProvidersFile::load()?,
        ProvidersFile::save,
        state_dir,
        name,
        def,
        api_key,
    )
}

fn save_endpoint_in(
    file: &mut ProvidersFile,
    persist: impl FnOnce(&ProvidersFile) -> Result<(), ProvidersFileError>,
    state_dir: &StateDir,
    name: &str,
    def: DecisionEndpointDef,
    api_key: Option<&str>,
) -> Result<SavedDecisionEndpoint, DecisionsError> {
    let endpoint = def.resolve(name)?;
    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        endpoint.api_key(Some(key.into()))?;
        save_decision_credentials(state_dir, name, key)?;
    }
    let added = !file.decision_endpoints().contains_key(name);
    file.upsert_decision_endpoint(name, &def)?;
    persist(file)?;
    let active = if added {
        persist_active_decision_endpoint(state_dir, Some(name))
            .map_err(DecisionsError::ActiveRow)?;
        true
    } else {
        read_active_decision_endpoint(state_dir).as_deref() == Some(name)
    };
    info!(endpoint = %name, protocol = endpoint.protocol.as_str(), added, active, "saved decision endpoint");
    Ok(SavedDecisionEndpoint {
        endpoint,
        added,
        active,
    })
}

/// Deletes `name`'s stored key and `providers.toml` table, and turns
/// decisions off when it was the active endpoint.
pub fn remove_decision_endpoint(
    state_dir: &StateDir,
    name: &str,
) -> Result<RemovedDecisionEndpoint, DecisionsError> {
    remove_endpoint_in(
        &mut ProvidersFile::load()?,
        ProvidersFile::save,
        state_dir,
        name,
    )
}

fn remove_endpoint_in(
    file: &mut ProvidersFile,
    persist: impl FnOnce(&ProvidersFile) -> Result<(), ProvidersFileError>,
    state_dir: &StateDir,
    name: &str,
) -> Result<RemovedDecisionEndpoint, DecisionsError> {
    let deleted_key = delete_decision_credentials(state_dir, name)?;
    let removed_table = file.remove_decision_endpoint(name);
    if removed_table {
        persist(file)?;
    }
    let deactivated = read_active_decision_endpoint(state_dir).as_deref() == Some(name);
    if deactivated {
        persist_active_decision_endpoint(state_dir, None).map_err(DecisionsError::ActiveRow)?;
    }
    info!(endpoint = %name, deleted_key, removed_table, deactivated, "removed decision endpoint");
    Ok(RemovedDecisionEndpoint {
        removed: deleted_key || removed_table,
        deactivated,
    })
}

/// Makes `name` the active endpoint once it resolves, or turns decisions off
/// for `None`. Answers the endpoint new sessions screen with.
pub fn use_decision_endpoint(
    state_dir: &StateDir,
    name: Option<&str>,
    providers: impl FnOnce() -> ProvidersConfig,
) -> Result<Option<DecisionEndpoint>, DecisionsError> {
    let endpoint = name
        .map(|name| {
            resolve_decision_endpoint(&providers(), name)?
                .ok_or_else(|| DecisionsError::UnknownEndpoint(name.into()))
        })
        .transpose()?;
    persist_active_decision_endpoint(state_dir, name).map_err(DecisionsError::ActiveRow)?;
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::{
        DECISION_PRESETS, DecisionKeySource, DecisionPreset, DecisionProtocol,
    };
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const MISSING: &str = "gone";
    const TYPESAFE: &str = "typesafe";
    const OPENAI: &str = "openai";
    const TUNNEL: &str = "tunnel";
    const TUNNEL_BASE_URL: &str = "http://100.64.0.2:8080/v1";
    const REMOTE_HTTP_BASE_URL: &str = "http://decisions.example.test";
    const NEW_KEY: &str = "new-decision-key";
    const STORED_KEY: &str = "stored-decision-key";
    const ENV_KEY: &str = "env-decision-key";
    const KEY_VARIABLE: &str = "TEST_CAUDRA_ACTIVE_DECISION_KEY";
    const PROVIDERS_UNREAD: &str = "providers.toml must not be read while decisions are off";

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    fn providers() -> ProvidersConfig {
        let mut providers = ProvidersConfig::default();
        for preset in &DECISION_PRESETS {
            let mut def = preset.def();
            def.api_key_env = Some(KEY_VARIABLE.into());
            providers.decisions.insert(preset.name.into(), def);
        }
        providers
    }

    #[test]
    fn no_active_row_is_off_without_reading_providers() {
        let (_tmp, dir) = state_dir();
        let config = resolve_active_decisions(&DecisionsConfig::default(), &dir, || {
            panic!("{PROVIDERS_UNREAD}")
        })
        .unwrap();
        assert!(config.endpoint.is_none());
    }

    #[test]
    fn missing_active_endpoint_is_off() {
        let (_tmp, dir) = state_dir();
        persist_active_decision_endpoint(&dir, Some(MISSING)).unwrap();
        let config =
            resolve_active_decisions(&DecisionsConfig::default(), &dir, providers).unwrap();
        assert!(config.endpoint.is_none());
    }

    #[test]
    fn active_endpoint_joins_the_shared_settings() {
        let (_tmp, dir) = state_dir();
        let preset = &DECISION_PRESETS[1];
        persist_active_decision_endpoint(&dir, Some(preset.name)).unwrap();
        let shared = DecisionsConfig {
            log: true,
            ..DecisionsConfig::default()
        };
        let config = resolve_active_decisions(&shared, &dir, providers).unwrap();
        let endpoint = config.endpoint.as_ref().unwrap();
        assert_eq!(endpoint.name, preset.name);
        assert_eq!(endpoint.protocol, preset.protocol);
        assert!(config.log);
    }

    #[test]
    fn invalid_active_endpoint_is_an_error() {
        let (_tmp, dir) = state_dir();
        let preset = &DECISION_PRESETS[0];
        persist_active_decision_endpoint(&dir, Some(preset.name)).unwrap();
        let mut broken = providers();
        broken.decisions.get_mut(preset.name).unwrap().base_url = "http://192.0.2.1".into();
        assert!(resolve_active_decisions(&DecisionsConfig::default(), &dir, || broken).is_err());
    }

    fn saved(
        file: &mut ProvidersFile,
        dir: &StateDir,
        name: &str,
        api_key: Option<&str>,
    ) -> SavedDecisionEndpoint {
        let def = DecisionPreset::find(name).map_or_else(
            || DecisionEndpointDef {
                protocol: DecisionProtocol::OpenAI,
                base_url: TUNNEL_BASE_URL.into(),
                model: None,
                api_key_env: None,
                allow_http: Some(true),
            },
            DecisionPreset::def,
        );
        save_endpoint_in(file, |_| Ok(()), dir, name, def, api_key).unwrap()
    }

    fn stored_key(dir: &StateDir, name: &str) -> Option<String> {
        load_decision_credentials(dir, name)
            .unwrap()
            .map(|credentials| credentials.api_key)
    }

    #[test]
    fn a_new_endpoint_becomes_active() {
        let (_tmp, dir) = state_dir();
        let mut file = ProvidersFile::default();
        let first = saved(&mut file, &dir, TYPESAFE, Some(STORED_KEY));
        let second = saved(&mut file, &dir, TUNNEL, None);
        assert!(first.added && first.active);
        assert!(second.added && second.active);
        assert_eq!(second.endpoint.name, TUNNEL);
        assert_eq!(read_active_decision_endpoint(&dir).as_deref(), Some(TUNNEL));
        assert_eq!(stored_key(&dir, TYPESAFE).as_deref(), Some(STORED_KEY));
        assert!(file.decision_endpoints().contains_key(TUNNEL));
    }

    #[test_case(TYPESAFE, true; "rekeying_the_active_one")]
    #[test_case(OPENAI, false; "rekeying_another_one")]
    fn rekeying_keeps_the_active_endpoint(rekeyed: &str, active: bool) {
        let (_tmp, dir) = state_dir();
        let mut file = ProvidersFile::default();
        saved(&mut file, &dir, OPENAI, Some(STORED_KEY));
        saved(&mut file, &dir, TYPESAFE, None);
        let result = saved(&mut file, &dir, rekeyed, Some(NEW_KEY));
        assert!(!result.added);
        assert_eq!(result.active, active);
        assert_eq!(
            read_active_decision_endpoint(&dir).as_deref(),
            Some(TYPESAFE)
        );
        assert_eq!(stored_key(&dir, rekeyed).as_deref(), Some(NEW_KEY));
    }

    #[test_case(None; "none")]
    #[test_case(Some("  "); "blank")]
    fn a_blank_key_keeps_the_stored_one(api_key: Option<&str>) {
        let (_tmp, dir) = state_dir();
        let mut file = ProvidersFile::default();
        saved(&mut file, &dir, TYPESAFE, Some(STORED_KEY));
        saved(&mut file, &dir, TYPESAFE, api_key);
        assert_eq!(stored_key(&dir, TYPESAFE).as_deref(), Some(STORED_KEY));
    }

    #[test]
    fn an_invalid_endpoint_saves_nothing() {
        let (_tmp, dir) = state_dir();
        let mut file = ProvidersFile::default();
        let mut def = DECISION_PRESETS[0].def();
        def.base_url = REMOTE_HTTP_BASE_URL.into();
        assert!(
            save_endpoint_in(&mut file, |_| Ok(()), &dir, TYPESAFE, def, Some(STORED_KEY)).is_err()
        );
        assert!(file.decision_endpoints().is_empty());
        assert!(stored_key(&dir, TYPESAFE).is_none());
        assert!(read_active_decision_endpoint(&dir).is_none());
    }

    #[test_case(TUNNEL, true; "removing_the_active_one")]
    #[test_case(TYPESAFE, false; "removing_another_one")]
    fn removing_clears_only_its_own_active_row(removed: &str, deactivated: bool) {
        let (_tmp, dir) = state_dir();
        let mut file = ProvidersFile::default();
        saved(&mut file, &dir, TYPESAFE, Some(STORED_KEY));
        saved(&mut file, &dir, TUNNEL, Some(STORED_KEY));
        let result = remove_endpoint_in(&mut file, |_| Ok(()), &dir, removed).unwrap();
        assert_eq!(
            result,
            RemovedDecisionEndpoint {
                removed: true,
                deactivated
            }
        );
        assert!(!file.decision_endpoints().contains_key(removed));
        assert!(stored_key(&dir, removed).is_none());
        assert_eq!(read_active_decision_endpoint(&dir).is_some(), !deactivated);
    }

    #[test]
    fn removing_an_unknown_endpoint_removes_nothing() {
        let (_tmp, dir) = state_dir();
        let result =
            remove_endpoint_in(&mut ProvidersFile::default(), |_| Ok(()), &dir, MISSING).unwrap();
        assert!(!result.removed && !result.deactivated);
    }

    #[test_case(Some(OPENAI), Some(OPENAI); "endpoint")]
    #[test_case(None, None; "off")]
    fn use_persists_the_active_row(name: Option<&str>, expected: Option<&str>) {
        let (_tmp, dir) = state_dir();
        persist_active_decision_endpoint(&dir, Some(TYPESAFE)).unwrap();
        let endpoint = use_decision_endpoint(&dir, name, providers).unwrap();
        assert_eq!(endpoint.map(|endpoint| endpoint.name).as_deref(), expected);
        assert_eq!(read_active_decision_endpoint(&dir).as_deref(), expected);
    }

    #[test]
    fn use_rejects_an_unknown_endpoint() {
        let (_tmp, dir) = state_dir();
        persist_active_decision_endpoint(&dir, Some(TYPESAFE)).unwrap();
        let error = use_decision_endpoint(&dir, Some(MISSING), providers).unwrap_err();
        assert!(matches!(error, DecisionsError::UnknownEndpoint(name) if name == MISSING));
        assert_eq!(
            read_active_decision_endpoint(&dir).as_deref(),
            Some(TYPESAFE)
        );
    }

    #[test_case(Some(STORED_KEY), Some(ENV_KEY), Some((STORED_KEY, DecisionKeySource::Stored)); "stored_over_env")]
    #[test_case(None, Some(ENV_KEY), Some((ENV_KEY, DecisionKeySource::Environment)); "env_fallback")]
    #[test_case(None, None, None; "keyless")]
    fn endpoint_key_prefers_the_saved_one(
        stored: Option<&str>,
        environment: Option<&str>,
        expected: Option<(&str, DecisionKeySource)>,
    ) {
        let (_tmp, dir) = state_dir();
        let preset = &DECISION_PRESETS[0];
        if let Some(key) = stored {
            save_decision_credentials(&dir, preset.name, key).unwrap();
        }
        match environment {
            Some(value) => unsafe { std::env::set_var(KEY_VARIABLE, value) },
            None => unsafe { std::env::remove_var(KEY_VARIABLE) },
        }
        let endpoint = resolve_decision_endpoint(&providers(), preset.name)
            .unwrap()
            .unwrap();
        let key = endpoint_api_key(&endpoint, &dir).unwrap();
        unsafe { std::env::remove_var(KEY_VARIABLE) };
        assert_eq!(
            key.map(|key| (key.value, key.source)),
            expected.map(|(value, source)| (value.to_owned(), source))
        );
    }
}
