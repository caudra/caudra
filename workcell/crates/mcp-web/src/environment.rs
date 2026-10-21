use std::str::FromStr;

use crate::{
    SerpApiEngine, WebsearchBackend, WebsearchConfigurationIssue, WebsearchExecutionConfiguration,
    WebsearchMode,
};

pub const WEBSEARCH_BACKEND_VARIABLE: &str = "WORKCELL_WEBSEARCH_BACKEND";
pub const WEBSEARCH_MODE_VARIABLE: &str = "WORKCELL_WEBSEARCH_MODE";
pub const SEARXNG_URL_VARIABLE: &str = "SEARXNG_URL";
pub const SEARXNG_API_KEY_VARIABLE: &str = "SEARXNG_API_KEY";
pub const SEARXNG_BEARER_TOKEN_VARIABLE: &str = "SEARXNG_BEARER_TOKEN";
pub const SEARXNG_USER_VARIABLE: &str = "SEARXNG_USER";
pub const SEARXNG_PASSWORD_VARIABLE: &str = "SEARXNG_PASSWORD";
pub const EXA_API_KEY_VARIABLE: &str = "EXA_API_KEY";
pub const BRAVE_API_KEY_VARIABLE: &str = "BRAVE_API_KEY";
pub const KAGI_API_KEY_VARIABLE: &str = "KAGI_API_KEY";
pub const SERPAPI_API_KEY_VARIABLE: &str = "SERPAPI_API_KEY";
pub const SERPAPI_ENGINE_VARIABLE: &str = "SERPAPI_ENGINE";
/// Every name web configuration may read, whichever backend is selected, so none reaches a
/// forwarded shell environment even when the active backend never consulted it.
pub const WEB_CONFIGURATION_VARIABLES: &[&str] = &[
    WEBSEARCH_BACKEND_VARIABLE,
    WEBSEARCH_MODE_VARIABLE,
    SEARXNG_URL_VARIABLE,
    SEARXNG_API_KEY_VARIABLE,
    SEARXNG_BEARER_TOKEN_VARIABLE,
    SEARXNG_USER_VARIABLE,
    SEARXNG_PASSWORD_VARIABLE,
    EXA_API_KEY_VARIABLE,
    BRAVE_API_KEY_VARIABLE,
    KAGI_API_KEY_VARIABLE,
    SERPAPI_API_KEY_VARIABLE,
    SERPAPI_ENGINE_VARIABLE,
];

const DISABLED_BACKEND: &str = "disabled";
const BACKEND_SEPARATOR: char = ',';

/// Resolve once at startup. Secret strings move directly into the web crate's
/// redacting immutable configuration and are never retained here.
pub fn resolve_web_configuration_with<F>(mut read: F) -> WebsearchExecutionConfiguration
where
    F: FnMut(&str) -> Result<Option<String>, ()>,
{
    let list = match read_normalized(&mut read, WEBSEARCH_BACKEND_VARIABLE) {
        Ok(Some(list)) => list,
        Ok(None) => return WebsearchExecutionConfiguration::exa_mcp(),
        Err(issue) => return WebsearchExecutionConfiguration::unavailable(None, issue),
    };
    if list == DISABLED_BACKEND {
        return WebsearchExecutionConfiguration::disabled(WebsearchBackend::ExaMcp);
    }
    let backends = match parse_backend_list(&list) {
        Ok(backends) => backends,
        Err(issue) => return WebsearchExecutionConfiguration::unavailable(None, issue),
    };
    let mode = match read_normalized(&mut read, WEBSEARCH_MODE_VARIABLE)
        .and_then(|mode| mode.map_or(Ok(WebsearchMode::default()), |mode| mode.parse()))
    {
        Ok(mode) => mode,
        Err(issue) => return WebsearchExecutionConfiguration::unavailable(None, issue),
    };
    let members = backends
        .into_iter()
        .map(|backend| resolve_backend(backend, &mut read))
        .collect();
    WebsearchExecutionConfiguration::combined(mode, members)
}

fn read_normalized<F>(
    read: &mut F,
    name: &str,
) -> Result<Option<String>, WebsearchConfigurationIssue>
where
    F: FnMut(&str) -> Result<Option<String>, ()>,
{
    read(name)
        .map(|value| {
            value
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
        })
        .map_err(|()| WebsearchConfigurationIssue::InvalidUnicode)
}

fn parse_backend_list(list: &str) -> Result<Vec<WebsearchBackend>, WebsearchConfigurationIssue> {
    let mut backends = Vec::new();
    for name in list.split(BACKEND_SEPARATOR).map(str::trim) {
        if name.is_empty() || name == DISABLED_BACKEND {
            return Err(WebsearchConfigurationIssue::InvalidBackendList);
        }
        let backend = WebsearchBackend::from_str(name)?;
        if backends.contains(&backend) {
            return Err(WebsearchConfigurationIssue::DuplicateBackend);
        }
        backends.push(backend);
    }
    Ok(backends)
}

fn resolve_backend<F>(backend: WebsearchBackend, read: &mut F) -> WebsearchExecutionConfiguration
where
    F: FnMut(&str) -> Result<Option<String>, ()>,
{
    match backend {
        WebsearchBackend::ExaMcp => WebsearchExecutionConfiguration::exa_mcp(),
        WebsearchBackend::Exa => provider_key(
            read(EXA_API_KEY_VARIABLE),
            backend,
            WebsearchExecutionConfiguration::exa,
            WebsearchExecutionConfiguration::exa_without_api_key,
        ),
        WebsearchBackend::Brave => provider_key(
            read(BRAVE_API_KEY_VARIABLE),
            backend,
            WebsearchExecutionConfiguration::brave,
            WebsearchExecutionConfiguration::brave_without_api_key,
        ),
        WebsearchBackend::Kagi => provider_key(
            read(KAGI_API_KEY_VARIABLE),
            backend,
            WebsearchExecutionConfiguration::kagi,
            WebsearchExecutionConfiguration::kagi_without_api_key,
        ),
        WebsearchBackend::Serpapi => resolve_serpapi(read),
        WebsearchBackend::Searxng => resolve_searxng(read),
    }
}

fn resolve_serpapi<F>(read: &mut F) -> WebsearchExecutionConfiguration
where
    F: FnMut(&str) -> Result<Option<String>, ()>,
{
    let invalid_unicode = || {
        WebsearchExecutionConfiguration::unavailable(
            Some(WebsearchBackend::Serpapi),
            WebsearchConfigurationIssue::InvalidUnicode,
        )
    };
    let Ok(key) = read(SERPAPI_API_KEY_VARIABLE).map(configured_secret) else {
        return invalid_unicode();
    };
    let Some(key) = key else {
        return WebsearchExecutionConfiguration::serpapi_without_api_key();
    };
    let Ok(engine) = read_normalized(read, SERPAPI_ENGINE_VARIABLE) else {
        return invalid_unicode();
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
    WebsearchExecutionConfiguration::serpapi(key, engine)
}

fn resolve_searxng<F>(read: &mut F) -> WebsearchExecutionConfiguration
where
    F: FnMut(&str) -> Result<Option<String>, ()>,
{
    let unavailable = |issue| {
        WebsearchExecutionConfiguration::unavailable(Some(WebsearchBackend::Searxng), issue)
    };
    macro_rules! read_searxng {
        ($name:expr) => {
            match read($name) {
                Ok(value) => value,
                Err(()) => return unavailable(WebsearchConfigurationIssue::InvalidUnicode),
            }
        };
    }

    let endpoint = read_searxng!(SEARXNG_URL_VARIABLE)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let api_key = configured_secret(read_searxng!(SEARXNG_API_KEY_VARIABLE));
    let bearer = configured_secret(read_searxng!(SEARXNG_BEARER_TOKEN_VARIABLE));
    let username = configured_secret(read_searxng!(SEARXNG_USER_VARIABLE));
    let password = configured_secret(read_searxng!(SEARXNG_PASSWORD_VARIABLE));

    if username.is_some() != password.is_some() {
        return unavailable(WebsearchConfigurationIssue::IncompleteBasicAuthentication);
    }
    if usize::from(api_key.is_some())
        + usize::from(bearer.is_some())
        + usize::from(username.is_some())
        > 1
    {
        return unavailable(WebsearchConfigurationIssue::AmbiguousSearxngCredentials);
    }
    let Some(endpoint) = endpoint else {
        return unavailable(WebsearchConfigurationIssue::MissingSearxngEndpoint);
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

    use test_case::test_case;

    use super::{
        BRAVE_API_KEY_VARIABLE, EXA_API_KEY_VARIABLE, SEARXNG_URL_VARIABLE,
        WEBSEARCH_BACKEND_VARIABLE, WEBSEARCH_MODE_VARIABLE, resolve_web_configuration_with,
    };
    use crate::{
        WebsearchBackend, WebsearchConfigurationIssue, WebsearchExecutionConfiguration,
        WebsearchMode,
    };

    const SECRET_CANARY: &str = "secret-canary";
    const SEARXNG_ENDPOINT: &str = "https://search.example.test";
    const READY_STATUS: &str = "ready";
    const UNAVAILABLE_STATUS: &str = "unavailable";

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
        assert_eq!(resolve(&[]).status(), READY_STATUS);
        assert_eq!(
            resolve(&[(WEBSEARCH_BACKEND_VARIABLE, "exa-mcp")]).backend(),
            Some(WebsearchBackend::ExaMcp)
        );
        assert_eq!(
            resolve(&[(WEBSEARCH_BACKEND_VARIABLE, "disabled")]).status(),
            "disabled"
        );
    }

    #[test]
    fn selects_configured_provider_without_exposing_its_secret() {
        let configuration = resolve(&[
            (WEBSEARCH_BACKEND_VARIABLE, "exa"),
            (EXA_API_KEY_VARIABLE, SECRET_CANARY),
        ]);
        assert_eq!(configuration.backend(), Some(WebsearchBackend::Exa));
        assert_eq!(configuration.status(), READY_STATUS);
        assert!(!format!("{configuration:?}").contains(SECRET_CANARY));
    }

    #[test]
    fn two_ready_backends_combine_and_fall_back_by_default() {
        let configuration = resolve(&[
            (WEBSEARCH_BACKEND_VARIABLE, " SearXNG , exa-mcp "),
            (SEARXNG_URL_VARIABLE, SEARXNG_ENDPOINT),
        ]);
        assert_eq!(configuration.status(), READY_STATUS);
        assert_eq!(configuration.mode(), WebsearchMode::Fallback);
        assert_eq!(configuration.backend(), Some(WebsearchBackend::Searxng));
        assert_eq!(
            configuration.ready_backends(),
            [WebsearchBackend::Searxng, WebsearchBackend::ExaMcp]
        );
        assert!(configuration.skipped().is_empty());
    }

    #[test_case("exa-mcp," ; "a trailing separator")]
    #[test_case("exa-mcp,,brave" ; "an empty entry")]
    #[test_case("disabled,exa-mcp" ; "disabled beside a backend")]
    #[test_case("exa-mcp, disabled" ; "a backend beside disabled")]
    fn a_malformed_list_is_an_invalid_backend_list(list: &str) {
        let configuration = resolve(&[(WEBSEARCH_BACKEND_VARIABLE, list)]);
        assert_eq!(configuration.status(), UNAVAILABLE_STATUS);
        assert_eq!(configuration.backend(), None);
        assert_eq!(
            configuration.issue(),
            Some(WebsearchConfigurationIssue::InvalidBackendList)
        );
    }

    #[test]
    fn a_repeated_backend_is_a_duplicate() {
        let configuration = resolve(&[(WEBSEARCH_BACKEND_VARIABLE, "exa-mcp,brave,EXA-MCP")]);
        assert_eq!(
            configuration.issue(),
            Some(WebsearchConfigurationIssue::DuplicateBackend)
        );
    }

    #[test_case("bing" ; "a single name")]
    #[test_case("exa-mcp,bing" ; "a list")]
    fn an_unknown_name_is_an_invalid_backend(list: &str) {
        let configuration = resolve(&[(WEBSEARCH_BACKEND_VARIABLE, list)]);
        assert_eq!(configuration.backend(), None);
        assert_eq!(
            configuration.issue(),
            Some(WebsearchConfigurationIssue::InvalidBackend)
        );
    }

    #[test_case("parallel" ; "an unknown mode")]
    #[test_case("single,all" ; "a list of modes")]
    fn an_unknown_mode_is_invalid(mode: &str) {
        let configuration = resolve(&[
            (WEBSEARCH_BACKEND_VARIABLE, "exa-mcp,brave"),
            (BRAVE_API_KEY_VARIABLE, SECRET_CANARY),
            (WEBSEARCH_MODE_VARIABLE, mode),
        ]);
        assert_eq!(configuration.backend(), None);
        assert_eq!(
            configuration.issue(),
            Some(WebsearchConfigurationIssue::InvalidMode)
        );
    }

    #[test_case(" ALL ", WebsearchMode::All ; "all ignoring case and whitespace")]
    #[test_case("fallback", WebsearchMode::Fallback ; "fallback")]
    #[test_case("", WebsearchMode::Fallback ; "a blank mode")]
    fn the_mode_is_parsed_case_insensitively(mode: &str, expected: WebsearchMode) {
        let configuration = resolve(&[
            (WEBSEARCH_BACKEND_VARIABLE, "exa-mcp,brave"),
            (BRAVE_API_KEY_VARIABLE, SECRET_CANARY),
            (WEBSEARCH_MODE_VARIABLE, mode),
        ]);
        assert_eq!(configuration.mode(), expected);
    }

    #[test]
    fn single_mode_uses_the_first_ready_backend_and_reports_the_unconfigured_one() {
        let configuration = resolve(&[
            (WEBSEARCH_BACKEND_VARIABLE, "brave,exa,exa-mcp"),
            (EXA_API_KEY_VARIABLE, SECRET_CANARY),
            (WEBSEARCH_MODE_VARIABLE, "single"),
        ]);
        assert_eq!(configuration.status(), READY_STATUS);
        assert_eq!(configuration.mode(), WebsearchMode::Single);
        assert_eq!(configuration.ready_backends(), [WebsearchBackend::Exa]);
        assert_eq!(
            configuration.skipped(),
            [(
                WebsearchBackend::Brave,
                WebsearchConfigurationIssue::MissingBraveApiKey
            )]
        );
        assert!(!format!("{configuration:?}").contains(SECRET_CANARY));
    }

    #[test]
    fn with_no_ready_backend_the_first_diagnostic_wins_and_the_rest_are_skipped() {
        let configuration = resolve(&[(WEBSEARCH_BACKEND_VARIABLE, "searxng,brave")]);
        assert_eq!(configuration.status(), UNAVAILABLE_STATUS);
        assert_eq!(configuration.backend(), Some(WebsearchBackend::Searxng));
        assert_eq!(
            configuration.issue(),
            Some(WebsearchConfigurationIssue::MissingSearxngEndpoint)
        );
        assert!(configuration.ready_backends().is_empty());
        assert_eq!(
            configuration.skipped(),
            [(
                WebsearchBackend::Brave,
                WebsearchConfigurationIssue::MissingBraveApiKey
            )]
        );
    }

    #[test_case("searxng", || WebsearchExecutionConfiguration::unavailable(Some(WebsearchBackend::Searxng), WebsearchConfigurationIssue::MissingSearxngEndpoint) ; "searxng without an endpoint")]
    #[test_case("brave", WebsearchExecutionConfiguration::brave_without_api_key ; "brave without a key")]
    #[test_case("exa", || WebsearchExecutionConfiguration::exa(SECRET_CANARY) ; "exa with a key")]
    fn one_listed_backend_resolves_exactly_as_before(
        backend: &str,
        expected: fn() -> WebsearchExecutionConfiguration,
    ) {
        let configuration = resolve(&[
            (WEBSEARCH_BACKEND_VARIABLE, backend),
            (EXA_API_KEY_VARIABLE, SECRET_CANARY),
        ]);
        assert_eq!(format!("{configuration:?}"), format!("{:?}", expected()));
        assert_eq!(configuration.mode(), WebsearchMode::Single);
        assert!(configuration.skipped().is_empty());
    }

    #[test]
    fn invalid_unicode_in_the_mode_is_reported() {
        let configuration = resolve_web_configuration_with(|name| match name {
            WEBSEARCH_BACKEND_VARIABLE => Ok(Some("exa-mcp".to_owned())),
            WEBSEARCH_MODE_VARIABLE => Err(()),
            _ => Ok(None),
        });
        assert_eq!(
            configuration.issue(),
            Some(WebsearchConfigurationIssue::InvalidUnicode)
        );
    }
}
