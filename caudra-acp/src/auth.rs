use std::env;
use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    AgentResponse, AuthMethod, AuthMethodAgent, AuthMethodTerminal, AuthenticateRequest,
    AuthenticateResponse, Error as AcpError, Meta, RequestId,
};
use caudra_providers::accounts::{invalidate, require_account};
use caudra_providers::model::Model;
use serde::Serialize;
use serde_json::json;

use crate::errors::{SHUTTING_DOWN, auth_required, internal, invalid_target, parse_params};
use crate::server::{Incoming, Server};

pub(crate) const AUTHENTICATE: &str = "authenticate";
pub(crate) const AUTH_METHOD_ID: &str = "caudra-login";
pub(crate) const UNKNOWN_AUTH_METHOD: &str = "unknown auth method";
/// Appended to the command the client launched `caudra acp` with.
pub(crate) const TERMINAL_LOGIN_ARG: &str = "--login";
/// The `_meta` key Zed reads a login command from on a plain agent method.
pub(crate) const META_TERMINAL_AUTH: &str = "terminal-auth";
pub(crate) const LOGIN_ARGS: [&str; 2] = ["auth", "login"];
const AUTH_METHOD_NAME: &str = "Log in";
const AUTH_METHOD_DESCRIPTION: &str =
    "Sign in to a model provider with `caudra auth login`, then authenticate again";
const LOGIN_LABEL: &str = "Caudra login";
const FALLBACK_COMMAND: &str = "caudra";

/// What the server reads of the user's accounts. Injected, so tests never read
/// the credentials of whoever runs them.
pub trait AcpAccounts: Send + Sync {
    /// `Err` says what to run to sign in to `model`'s provider.
    fn check(&self, model: &Model) -> Result<(), String>;
    /// The model a session opens on by default, resolved as of now.
    fn default_model(&self) -> Result<Model, String>;
    /// The models the signed-in providers offer without a network call.
    fn model_specs(&self) -> Vec<String>;
}

/// Whether `model`'s provider has an account to sign with, reading what is
/// stored and configured now. A keyless or local provider always has one.
pub fn check_credentials(model: &Model) -> Result<(), String> {
    require_account(&model.provider).map_err(|error| error.user_message())
}

/// The one way to sign in: `caudra auth login`. A client that runs terminal
/// methods relaunches `caudra acp` with [`TERMINAL_LOGIN_ARG`]; any other gets
/// the full command under `_meta["terminal-auth"]`, Zed's older convention.
pub(crate) fn auth_method(terminal: bool) -> AuthMethod {
    if terminal {
        return AuthMethod::Terminal(
            AuthMethodTerminal::new(AUTH_METHOD_ID, AUTH_METHOD_NAME)
                .description(AUTH_METHOD_DESCRIPTION.to_owned())
                .args(vec![TERMINAL_LOGIN_ARG.to_owned()]),
        );
    }
    let command = env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| FALLBACK_COMMAND.to_owned());
    AuthMethod::Agent(
        AuthMethodAgent::new(AUTH_METHOD_ID, AUTH_METHOD_NAME)
            .description(AUTH_METHOD_DESCRIPTION.to_owned())
            .meta(Meta::from_iter([(
                META_TERMINAL_AUTH.to_owned(),
                json!(TerminalAuth {
                    command,
                    args: LOGIN_ARGS,
                    label: LOGIN_LABEL,
                }),
            )])),
    )
}

#[derive(Serialize)]
struct TerminalAuth {
    command: String,
    args: [&'static str; 2],
    label: &'static str,
}

pub(crate) struct SignedIn {
    pub(crate) model: Model,
    pub(crate) specs: Vec<String>,
}

pub(crate) struct AuthOutcome {
    pub(crate) request_id: RequestId,
    pub(crate) result: Result<SignedIn, AcpError>,
}

/// Re-reads every provider's accounts, then resolves the default model again,
/// since a first sign-in can be what makes one resolvable at all.
fn sign_in(accounts: &dyn AcpAccounts) -> Result<SignedIn, AcpError> {
    invalidate(None);
    let model = accounts
        .default_model()
        .map_err(|text| auth_required(&text))?;
    accounts
        .check(&model)
        .map_err(|text| auth_required(&text))?;
    Ok(SignedIn {
        specs: accounts.model_specs(),
        model,
    })
}

impl Server {
    /// Reading accounts blocks on storage and may run a provider's auth
    /// script, so it runs off the main loop and reports back.
    pub(crate) fn authenticate(&mut self, id: RequestId, raw: &serde_json::Value) {
        let request: AuthenticateRequest = match parse_params(raw) {
            Ok(request) => request,
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        if request.method_id.0.as_ref() != AUTH_METHOD_ID {
            let error = invalid_target(&format!("{UNKNOWN_AUTH_METHOD}: {}", request.method_id));
            return self.outbound.respond(id, Err(error));
        }
        let Some(events) = self.events.upgrade() else {
            return self.outbound.respond(id, Err(internal(SHUTTING_DOWN)));
        };
        let accounts = Arc::clone(&self.params.accounts);
        smol::spawn(async move {
            let result = smol::unblock(move || sign_in(accounts.as_ref())).await;
            let outcome = AuthOutcome {
                request_id: id,
                result,
            };
            let _ = events.send(Incoming::Authenticated(Box::new(outcome)));
        })
        .detach();
    }

    /// New sessions open on the model the sign-in resolved, and the models it
    /// made available join the list.
    pub(crate) fn settle_authenticate(&mut self, outcome: AuthOutcome) {
        match outcome.result {
            Ok(SignedIn { model, specs }) => {
                self.default_model = model;
                self.outbound.respond(
                    outcome.request_id,
                    Ok(AgentResponse::AuthenticateResponse(
                        AuthenticateResponse::new(),
                    )),
                );
                self.refresh_models(specs);
            }
            Err(error) => self.outbound.respond(outcome.request_id, Err(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::ErrorCode;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;
    use crate::catalog::MODELS_CHANGED;
    use crate::errors::DATA_DETAILS;
    use crate::test_support::{LOGIN_MISSING, OFFLINE_SPEC, TestServer};

    const AUTHENTICATE_ID: i64 = 41;
    const KEYLESS_SPEC: &str = "ollama/llama3.2";
    const UNKNOWN_METHOD: &str = "oauth-device";

    fn authenticate(method_id: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": AUTHENTICATE_ID,
            "method": AUTHENTICATE,
            "params": {"methodId": method_id},
        })
    }

    #[test_case(true ; "terminal_client")]
    #[test_case(false ; "legacy_client")]
    fn the_login_method_fits_what_the_client_runs(terminal: bool) {
        let json = serde_json::to_value(auth_method(terminal)).unwrap();

        assert_eq!(json["id"], AUTH_METHOD_ID);
        if terminal {
            assert_eq!(json["type"], "terminal");
            assert_eq!(json["args"], json!([TERMINAL_LOGIN_ARG]));
            assert!(json.get("_meta").is_none(), "{json}");
        } else {
            assert!(
                json.get("type").is_none(),
                "an agent method has no type: {json}"
            );
            let login = &json["_meta"][META_TERMINAL_AUTH];
            assert!(
                login["command"]
                    .as_str()
                    .is_some_and(|command| !command.is_empty())
            );
            assert_eq!(login["args"], json!(LOGIN_ARGS));
            assert_eq!(login["label"], LOGIN_LABEL);
        }
    }

    #[test]
    fn authenticating_after_a_sign_in_succeeds_and_adopts_the_default_model() {
        let mut test = TestServer::signed_out();
        test.accounts.sign_in();

        test.server.handle_message(authenticate(AUTH_METHOD_ID));
        test.settle_next();

        let response = test.next_message();
        assert_eq!(response["id"], AUTHENTICATE_ID);
        assert!(response["result"].is_object(), "{response}");
        assert_eq!(test.server.default_model.spec(), OFFLINE_SPEC);
        let changed = test.next_message();
        assert_eq!(changed["method"], MODELS_CHANGED);
        assert_eq!(changed["params"]["defaultModel"], OFFLINE_SPEC);
        assert_eq!(changed["params"]["models"][0]["id"], OFFLINE_SPEC);
    }

    #[test]
    fn authenticating_while_still_signed_out_is_auth_required_with_details() {
        let mut test = TestServer::signed_out();

        test.server.handle_message(authenticate(AUTH_METHOD_ID));
        test.settle_next();

        let error = &test.next_message()["error"];
        assert_eq!(error["code"], i32::from(ErrorCode::AuthRequired));
        assert_eq!(error["message"], LOGIN_MISSING);
        assert_eq!(error["data"][DATA_DETAILS], LOGIN_MISSING);
    }

    #[test]
    fn an_unknown_auth_method_is_invalid_params() {
        let mut test = TestServer::initialized();

        test.server.handle_message(authenticate(UNKNOWN_METHOD));

        let error = &test.next_message()["error"];
        assert_eq!(error["code"], i32::from(ErrorCode::InvalidParams));
        assert_eq!(
            error["message"],
            format!("{UNKNOWN_AUTH_METHOD}: {UNKNOWN_METHOD}")
        );
    }

    /// A local server needs no key, so it never asks for a sign-in whatever
    /// is stored on the machine running the test.
    #[test]
    fn a_keyless_provider_has_credentials() {
        let model = Model::from_spec(KEYLESS_SPEC).unwrap();

        assert_eq!(check_credentials(&model), Ok(()));
    }
}
