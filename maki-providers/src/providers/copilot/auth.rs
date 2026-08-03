use std::env;
use std::fs;
use std::path::PathBuf;

use base64::Engine;
use keyring::Entry;
use maki_storage::StateDir;
use maki_storage::auth::{
    ProviderCredentials, delete_provider_credentials, load_provider_credentials,
    save_provider_credentials,
};
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use tracing::{debug, warn};

use crate::AgentError;

const TOKEN_ENV_VARS: &[&str] = &["GH_COPILOT_TOKEN", "COPILOT_GITHUB_TOKEN"];
const DEFAULT_HOST: &str = "github.com";
const PROVIDER: &str = "copilot";
const KEYRING_SERVICE_PREFIX: &str = "gh:";
const GO_KEYRING_B64_PREFIX: &str = "go-keyring-base64:";
const KEYRING_LOCKED_MESSAGE: &str = "is unreadable; is the store locked?";

pub(crate) fn graphql_url(host: &str) -> String {
    if host == DEFAULT_HOST {
        "https://api.github.com/graphql".to_owned()
    } else {
        format!("https://{host}/api/graphql")
    }
}

pub(crate) fn load_token() -> Result<ProviderCredentials, AgentError> {
    for key in TOKEN_ENV_VARS {
        if let Ok(token) = env::var(key)
            && !token.trim().is_empty()
        {
            return Ok(ProviderCredentials {
                api_key: token,
                host: None,
            });
        }
    }

    if let Ok(dir) = StateDir::resolve()
        && let Some(creds) = load_provider_credentials(&dir, PROVIDER)
    {
        debug!("using saved Copilot credentials");
        return Ok(creds);
    }

    Err(AgentError::Config {
        message: "not authenticated, run `maki auth login copilot` or set GH_COPILOT_TOKEN".into(),
    })
}

fn discover_token(hints: &mut Vec<String>) -> Result<ProviderCredentials, AgentError> {
    for path in copilot_config_paths() {
        if let Ok(contents) = fs::read_to_string(path)
            && let Some((token, host)) = extract_oauth_token_json(&contents)
        {
            return Ok(ProviderCredentials {
                api_key: token,
                host: (host != DEFAULT_HOST).then_some(host),
            });
        }
    }

    for path in gh_config_paths() {
        if let Ok(contents) = fs::read_to_string(path)
            && let Some((token, host)) = extract_oauth_token_yaml(&contents)
        {
            return Ok(ProviderCredentials {
                api_key: token,
                host: (host != DEFAULT_HOST).then_some(host),
            });
        }
    }

    if let Some((token, host)) = discover_keyring_token(hints) {
        return Ok(ProviderCredentials {
            api_key: token,
            host: (host != DEFAULT_HOST).then_some(host),
        });
    }

    Err(AgentError::Config {
        message: "Copilot token not found. Run `gh auth login --web`, sign in with the Copilot \
            client, or set GH_COPILOT_TOKEN."
            .into(),
    })
}

fn discover_keyring_token(hints: &mut Vec<String>) -> Option<(String, String)> {
    let files = readable_config_files();
    for host in keyring_hosts(&files) {
        for account in keyring_accounts(&files, &host) {
            let Ok(entry) = Entry::new(&format!("{KEYRING_SERVICE_PREFIX}{host}"), &account) else {
                debug!(host, account, "gh keyring account rejected");
                continue;
            };
            match entry.get_password() {
                Ok(raw) => return Some((decode_go_keyring(&raw), host)),
                Err(keyring::Error::NoEntry) => {}
                Err(err) => {
                    warn!(
                        error = %err, host, account,
                        "gh keyring entry {}", KEYRING_LOCKED_MESSAGE
                    );
                    hints.push(locked_keyring_hint(&host, &account, err));
                }
            }
        }
    }
    None
}

fn locked_keyring_hint(host: &str, account: &str, error: impl std::fmt::Display) -> String {
    format!("gh keyring entry for {host}/{account} {KEYRING_LOCKED_MESSAGE} ({error})")
}

fn keyring_accounts(files: &[String], host: &str) -> Vec<String> {
    let mut accounts = Vec::new();
    for contents in files {
        for user in config_usernames(contents, host) {
            if !accounts.contains(&user) {
                accounts.push(user);
            }
        }
    }
    push_unique(&mut accounts, "");
    accounts
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|item| item == value) {
        list.push(value.to_owned());
    }
}

// YAML 1.2 is a superset of JSON, so one serde_yaml parse covers both the JSON copilot config and
// the YAML gh config.
fn config_usernames(contents: &str, host: &str) -> Vec<String> {
    let mut usernames = Vec::new();
    if let Ok(value) = serde_yaml::from_str::<YamlValue>(contents)
        && let Some(cfg) = value.get(host).and_then(YamlValue::as_mapping)
    {
        if let Some(user) = cfg.get("user").and_then(YamlValue::as_str) {
            push_unique(&mut usernames, user);
        }
        if let Some(users) = cfg.get("users").and_then(YamlValue::as_mapping) {
            for key in users.keys().filter_map(|key| key.as_str()) {
                push_unique(&mut usernames, key);
            }
        }
    }
    usernames
}

fn keyring_hosts(files: &[String]) -> Vec<String> {
    let mut hosts = vec![DEFAULT_HOST.to_owned()];
    for contents in files {
        for host in config_file_hosts(contents) {
            if is_github_host(&host) && !hosts.contains(&host) {
                hosts.push(host);
            }
        }
    }
    hosts
}

fn readable_config_files() -> Vec<String> {
    copilot_config_paths()
        .into_iter()
        .chain(gh_config_paths())
        .filter_map(|path| fs::read_to_string(path).ok())
        .collect()
}

// YAML 1.2 is a superset of JSON; also parses the JSON copilot config.
fn config_file_hosts(contents: &str) -> Vec<String> {
    let Ok(value) = serde_yaml::from_str::<YamlValue>(contents) else {
        return Vec::new();
    };
    value
        .as_mapping()
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Reverses the encoding written by the `go-keyring` library (used by the gh CLI,
/// which is written in Go): it base64-encodes the token bytes and prefixes them
/// with `go-keyring-base64:`. Plain (un-prefixed) tokens are returned as-is.
fn decode_go_keyring(raw: &str) -> String {
    raw.strip_prefix(GO_KEYRING_B64_PREFIX)
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| raw.to_owned())
}

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    if load_token().is_ok() {
        println!("Already authenticated with Copilot.");
        return Ok(());
    }

    let mut hints = Vec::new();
    let creds = match discover_token(&mut hints) {
        Ok(creds) => creds,
        Err(err) => {
            for hint in hints {
                eprintln!("{hint}");
            }
            return Err(err);
        }
    };
    let host = creds.host.as_deref().unwrap_or(DEFAULT_HOST);
    println!("Copilot token imported from gh CLI / Copilot client / system keyring ({host}).");
    save_provider_credentials(dir, PROVIDER, &creds)?;
    Ok(())
}

pub fn logout(dir: &StateDir) -> Result<(), AgentError> {
    if delete_provider_credentials(dir, PROVIDER)? {
        println!("Logged out of Copilot.");
    } else {
        println!("Not currently logged in to Copilot.");
    }
    Ok(())
}

fn is_github_host(host: &str) -> bool {
    host == DEFAULT_HOST || host.ends_with(".ghe.com") || host.ends_with(".github.com")
}

fn copilot_config_paths() -> Vec<PathBuf> {
    config_dir()
        .map(|config| config.join("github-copilot"))
        .map(|base| vec![base.join("hosts.json"), base.join("apps.json")])
        .unwrap_or_default()
}

fn gh_config_paths() -> Vec<PathBuf> {
    config_dir()
        .map(|config| vec![config.join("gh").join("hosts.yml")])
        .unwrap_or_default()
}

fn config_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| maki_storage::paths::home().map(|home| home.join(".config")))
}

fn extract_oauth_token_json(contents: &str) -> Option<(String, String)> {
    let value: JsonValue = serde_json::from_str(contents).ok()?;
    value.as_object()?.iter().find_map(|(key, value)| {
        if is_github_host(key) {
            value["oauth_token"]
                .as_str()
                .map(|tok| (tok.to_owned(), key.clone()))
        } else {
            None
        }
    })
}

fn extract_oauth_token_yaml(contents: &str) -> Option<(String, String)> {
    let value: YamlValue = serde_yaml::from_str(contents).ok()?;
    value.as_mapping()?.iter().find_map(|(key, value)| {
        let host = key.as_str()?;
        if is_github_host(host) {
            value["oauth_token"]
                .as_str()
                .map(|tok| (tok.to_owned(), host.to_owned()))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn locked_keyring_hint_includes_host_account_and_error() {
        let hint = locked_keyring_hint("github.com", "janedoe", "access denied");
        assert!(hint.contains("github.com"));
        assert!(hint.contains("janedoe"));
        assert!(hint.contains(KEYRING_LOCKED_MESSAGE));
        assert!(hint.ends_with("(access denied)"));
    }

    #[test_case(
        r#"{"github.com": {"oauth_token": "token-1"}}"# => Some(("token-1".to_string(), "github.com".to_string())); "json_matching_domain"
    )]
    #[test_case(
        r#"{"enterprise.example.com": {"oauth_token": "token-1"}}"# => None; "json_other_domain"
    )]
    #[test_case(
        r#"{"myco.ghe.com": {"oauth_token": "ghe-tok"}}"# => Some(("ghe-tok".to_string(), "myco.ghe.com".to_string())); "json_ghe_host"
    )]
    fn extract_json_token(contents: &str) -> Option<(String, String)> {
        extract_oauth_token_json(contents)
    }

    #[test_case(
        "github.com:\n  oauth_token: token-1\n  user: octocat\n" => Some(("token-1".to_string(), "github.com".to_string())); "yaml_matching_domain"
    )]
    #[test_case(
        "enterprise.example.com:\n  oauth_token: token-1\n" => None; "yaml_other_domain"
    )]
    #[test_case(
        "myco.ghe.com:\n  oauth_token: ghe-tok\n" => Some(("ghe-tok".to_string(), "myco.ghe.com".to_string())); "yaml_ghe_host"
    )]
    fn extract_yaml_token(contents: &str) -> Option<(String, String)> {
        extract_oauth_token_yaml(contents)
    }

    #[test_case("github.com" => true; "github_com")]
    #[test_case("myco.ghe.com" => true; "ghe_com")]
    #[test_case("gitlab.com" => false; "gitlab")]
    #[test_case("evil-ghe.com" => false; "fake_ghe")]
    fn test_is_github_host(host: &str) -> bool {
        is_github_host(host)
    }

    #[test_case("github.com" => "https://api.github.com/graphql"; "graphql_default")]
    #[test_case("myco.ghe.com" => "https://myco.ghe.com/api/graphql"; "graphql_ghe")]
    fn test_graphql_url(host: &str) -> String {
        graphql_url(host)
    }

    #[test_case("go-keyring-base64:Z2hvX3Rva2Vu" => "gho_token".to_owned(); "go_keyring_prefixed")]
    #[test_case("gho_token" => "gho_token".to_owned(); "plain_token")]
    #[test_case("go-keyring-base64:!!!not-base64" => "go-keyring-base64:!!!not-base64".to_owned(); "invalid_base64_kept_raw")]
    fn test_decode_go_keyring(raw: &str) -> String {
        decode_go_keyring(raw)
    }

    #[test_case(
        r#"{"github.com": {"oauth_token": "t"}}"# => vec!["github.com".to_owned()]; "json_hosts"
    )]
    #[test_case(
        "github.com:\n  oauth_token: t\nmyco.ghe.com:\n  oauth_token: t\n" =>
        vec!["github.com".to_owned(), "myco.ghe.com".to_owned()]; "yaml_hosts"
    )]
    #[test_case("[unclosed" => Vec::<String>::new(); "unparsable")]
    fn test_config_file_hosts(contents: &str) -> Vec<String> {
        config_file_hosts(contents)
    }

    #[test_case(
        r#"{"github.com": {"user": "janedoe", "oauth_token": "t"}}"#, "github.com" =>
        vec!["janedoe".to_owned()]; "json_user"
    )]
    #[test_case(
        "github.com:\n  user: janedoe\n  users:\n    janedoe:\n    janesmith_second:\n",
        "github.com" =>
        vec!["janedoe".to_owned(), "janesmith_second".to_owned()]; "yaml_user_and_users"
    )]
    #[test_case(
        r#"{"github.com": {"user": "janedoe", "users": {"janedoe": {}, "janesmith": {}}}}"#,
        "github.com" =>
        vec!["janedoe".to_owned(), "janesmith".to_owned()]; "json_user_and_users"
    )]
    #[test_case(
        "github.com:\n  user: janedoe\n", "myco.ghe.com" =>
        Vec::<String>::new(); "missing_host"
    )]
    fn test_config_usernames(contents: &str, host: &str) -> Vec<String> {
        config_usernames(contents, host)
    }

    #[test_case(
        vec![r#"{"github.com": {"oauth_token": "t"}}"#.to_owned()], "github.com" =>
        vec!["".to_owned()]; "host_without_username_gets_empty_account")]
    #[test_case(
        vec!["github.com:\n  user: janedoe\n".to_owned()], "github.com" =>
        vec!["janedoe".to_owned(), "".to_owned()]; "usernames_then_empty_account_last")]
    fn test_keyring_accounts(files: Vec<String>, host: &str) -> Vec<String> {
        keyring_accounts(&files, host)
    }
}
