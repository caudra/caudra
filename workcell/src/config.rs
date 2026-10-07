use workcell_mcp_web::{
    SerpApiEngine, WebsearchBackend, WebsearchConfigurationIssue, WebsearchExecutionConfiguration,
};

const BACKEND: &str = "WORKCELL_WEBSEARCH_BACKEND";
const SEARXNG_URL: &str = "SEARXNG_URL";
const SEARXNG_API_KEY: &str = "SEARXNG_API_KEY";
const SEARXNG_BEARER_TOKEN: &str = "SEARXNG_BEARER_TOKEN";
const SEARXNG_USER: &str = "SEARXNG_USER";
const SEARXNG_PASSWORD: &str = "SEARXNG_PASSWORD";
const EXA_API_KEY: &str = "EXA_API_KEY";
const BRAVE_API_KEY: &str = "BRAVE_API_KEY";
const KAGI_API_KEY: &str = "KAGI_API_KEY";
const SERPAPI_API_KEY: &str = "SERPAPI_API_KEY";
const SERPAPI_ENGINE: &str = "SERPAPI_ENGINE";

/// Resolve once at startup. Secret strings move directly into the web crate's
/// redacting immutable configuration and are never retained here.
pub fn resolve_web_configuration_with<F>(mut read: F) -> WebsearchExecutionConfiguration
where
    F: FnMut(&str) -> Result<Option<String>, ()>,
{
    let backend = match read(BACKEND) {
        Ok(value) => value,
        Err(()) => {
            return WebsearchExecutionConfiguration::unavailable(
                None,
                WebsearchConfigurationIssue::InvalidUnicode,
            );
        }
    }
    .map(|value| value.trim().to_ascii_lowercase())
    .filter(|value| !value.is_empty());
    let Some(backend) = backend else {
        return WebsearchExecutionConfiguration::exa_mcp();
    };
    if !matches!(
        backend.as_str(),
        "disabled" | "exa-mcp" | "searxng" | "exa" | "brave" | "kagi" | "serpapi"
    ) {
        return WebsearchExecutionConfiguration::unavailable(
            None,
            WebsearchConfigurationIssue::InvalidBackend,
        );
    }

    if backend == "disabled" {
        return WebsearchExecutionConfiguration::disabled(WebsearchBackend::ExaMcp);
    }
    if backend == "exa-mcp" {
        return WebsearchExecutionConfiguration::exa_mcp();
    }

    if backend == "exa" {
        return provider_key(
            read(EXA_API_KEY),
            WebsearchBackend::Exa,
            WebsearchExecutionConfiguration::exa,
            WebsearchExecutionConfiguration::exa_without_api_key,
        );
    }
    if backend == "brave" {
        return provider_key(
            read(BRAVE_API_KEY),
            WebsearchBackend::Brave,
            WebsearchExecutionConfiguration::brave,
            WebsearchExecutionConfiguration::brave_without_api_key,
        );
    }
    if backend == "kagi" {
        return provider_key(
            read(KAGI_API_KEY),
            WebsearchBackend::Kagi,
            WebsearchExecutionConfiguration::kagi,
            WebsearchExecutionConfiguration::kagi_without_api_key,
        );
    }
    if backend == "serpapi" {
        let key = match read(SERPAPI_API_KEY) {
            Ok(value) => configured_secret(value),
            Err(()) => {
                return WebsearchExecutionConfiguration::unavailable(
                    Some(WebsearchBackend::Serpapi),
                    WebsearchConfigurationIssue::InvalidUnicode,
                );
            }
        };
        let Some(key) = key else {
            return WebsearchExecutionConfiguration::serpapi_without_api_key();
        };
        let engine = match read(SERPAPI_ENGINE) {
            Ok(value) => value
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty()),
            Err(()) => {
                return WebsearchExecutionConfiguration::unavailable(
                    Some(WebsearchBackend::Serpapi),
                    WebsearchConfigurationIssue::InvalidUnicode,
                );
            }
        };
        let Some(engine) = engine else {
            return WebsearchExecutionConfiguration::serpapi_without_engine();
        };
        let engine = match engine.as_str() {
            "google" => SerpApiEngine::Google,
            "bing" => SerpApiEngine::Bing,
            _ => {
                return WebsearchExecutionConfiguration::unavailable(
                    Some(WebsearchBackend::Serpapi),
                    WebsearchConfigurationIssue::InvalidSerpApiEngine,
                );
            }
        };
        return WebsearchExecutionConfiguration::serpapi(key, engine);
    }

    macro_rules! read_searxng {
        ($name:expr) => {
            match read($name) {
                Ok(value) => value,
                Err(()) => {
                    return WebsearchExecutionConfiguration::unavailable(
                        Some(WebsearchBackend::Searxng),
                        WebsearchConfigurationIssue::InvalidUnicode,
                    );
                }
            }
        };
    }

    let endpoint = read_searxng!(SEARXNG_URL)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let api_key = configured_secret(read_searxng!(SEARXNG_API_KEY));
    let bearer = configured_secret(read_searxng!(SEARXNG_BEARER_TOKEN));
    let username = configured_secret(read_searxng!(SEARXNG_USER));
    let password = configured_secret(read_searxng!(SEARXNG_PASSWORD));

    if username.is_some() != password.is_some() {
        return WebsearchExecutionConfiguration::unavailable(
            Some(WebsearchBackend::Searxng),
            WebsearchConfigurationIssue::IncompleteBasicAuthentication,
        );
    }
    if usize::from(api_key.is_some())
        + usize::from(bearer.is_some())
        + usize::from(username.is_some())
        > 1
    {
        return WebsearchExecutionConfiguration::unavailable(
            Some(WebsearchBackend::Searxng),
            WebsearchConfigurationIssue::AmbiguousSearxngCredentials,
        );
    }
    let Some(endpoint) = endpoint else {
        return WebsearchExecutionConfiguration::unavailable(
            Some(WebsearchBackend::Searxng),
            WebsearchConfigurationIssue::MissingSearxngEndpoint,
        );
    };
    match (api_key, bearer, username, password) {
        (Some(key), None, None, None) => {
            WebsearchExecutionConfiguration::searxng_api_key(endpoint, key)
        }
        (None, Some(token), None, None) => {
            WebsearchExecutionConfiguration::searxng_bearer(endpoint, token)
        }
        (None, None, Some(user), Some(password)) => {
            WebsearchExecutionConfiguration::searxng_basic(endpoint, user, password)
        }
        (None, None, None, None) => WebsearchExecutionConfiguration::searxng(endpoint),
        _ => unreachable!("credential ambiguity and completeness checked above"),
    }
}

fn provider_key(
    value: Result<Option<String>, ()>,
    backend: WebsearchBackend,
    configured: fn(String) -> WebsearchExecutionConfiguration,
    missing: fn() -> WebsearchExecutionConfiguration,
) -> WebsearchExecutionConfiguration {
    match value {
        Ok(value) => configured_secret(value).map_or_else(missing, configured),
        Err(()) => WebsearchExecutionConfiguration::unavailable(
            Some(backend),
            WebsearchConfigurationIssue::InvalidUnicode,
        ),
    }
}

fn configured_secret(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn resolve(values: &[(&str, &str)]) -> WebsearchExecutionConfiguration {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        resolve_web_configuration_with(|name| Ok(values.get(name).cloned()))
    }

    #[test]
    fn defaults_to_credential_free_exa_mcp() {
        assert_eq!(resolve(&[]).backend(), Some(WebsearchBackend::ExaMcp));
        assert_eq!(resolve(&[]).status(), "ready");
        assert_eq!(
            resolve(&[(BACKEND, "exa-mcp")]).backend(),
            Some(WebsearchBackend::ExaMcp)
        );
        assert_eq!(resolve(&[(BACKEND, "disabled")]).status(), "disabled");
    }

    #[test]
    fn selects_configured_provider_without_exposing_its_secret() {
        let configuration = resolve(&[(BACKEND, "exa"), (EXA_API_KEY, "secret-canary")]);
        assert_eq!(configuration.backend(), Some(WebsearchBackend::Exa));
        assert_eq!(configuration.status(), "ready");
        assert!(!format!("{configuration:?}").contains("canary"));
    }
}
