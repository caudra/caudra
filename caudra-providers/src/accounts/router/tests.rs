use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use caudra_storage::accounts::{AccountId, AccountIdentity, AccountName};
use caudra_storage::auth::{OAuthTokens, now_millis};
use flume::Receiver;
use test_case::test_case;

use super::*;
use crate::accounts::credential::{AccountSecret, CredentialSource};
use crate::accounts::quota::{QuotaWindow, WindowKind};
use crate::accounts::registry::ProviderFactory;
use crate::accounts::registry::test_support::{slot, snapshot};
use crate::accounts::route::SelectionMode;

const SLUG: &str = "test-provider";
const PRIMARY: &str = "primary";
const SECONDARY: &str = "secondary";
const SESSION: &str = "session";
const CHILD: &str = "session/task";
const OTHER_SESSION: &str = "other";
const MODEL_SPEC: &str = "anthropic/claude-opus-4-8";
const SPENT_PERCENT: f32 = 97.0;
const HOUR_MS: u64 = 60 * 60 * 1000;
const LONG_WAIT: Duration = Duration::from_secs(120);
const SHORT_WAIT: Duration = Duration::from_secs(10);
const REFUSED: &str = "refused";

#[derive(Default)]
struct Script {
    replies: HashMap<String, VecDeque<StreamResponse>>,
    served: Vec<String>,
}

struct FakeAccount {
    name: String,
    script: Arc<Mutex<Script>>,
}

impl Provider for FakeAccount {
    fn stream_message<'a>(
        &'a self,
        _model: &'a Model,
        _messages: &'a [Message],
        _system: &'a str,
        _tools: &'a Value,
        _event_tx: &'a Sender<ProviderEvent>,
        _opts: RequestOptions,
        _cache_key: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let mut script = self.script.lock().unwrap();
            script.served.push(self.name.clone());
            Ok(script
                .replies
                .get_mut(&self.name)
                .and_then(VecDeque::pop_front)
                .unwrap_or_default())
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

struct Harness {
    router: RoutedProvider,
    state: Arc<SlugState>,
    script: Arc<Mutex<Script>>,
    model: Model,
    events_tx: Sender<ProviderEvent>,
    events: Receiver<ProviderEvent>,
}

impl Harness {
    fn new(accounts: &[&str], selection: AccountSelection) -> Self {
        let script = Arc::new(Mutex::new(Script::default()));
        let factory_script = Arc::clone(&script);
        let factory: ProviderFactory = Arc::new(move |credential: &AccountCredential, _| {
            Ok(Box::new(FakeAccount {
                name: credential.id.name.to_string(),
                script: Arc::clone(&factory_script),
            }) as Box<dyn Provider>)
        });
        let slots = accounts
            .iter()
            .map(|name| slot(subscription(name), false))
            .collect();
        let state = Arc::new(SlugState::fixed(SLUG, factory, snapshot(slots, selection)));
        let router = RoutedProvider::with_state(Arc::clone(&state), Timeouts::default()).unwrap();
        let (events_tx, events) = flume::unbounded();
        Self {
            router,
            state,
            script,
            model: Model::from_spec(MODEL_SPEC).unwrap(),
            events_tx,
            events,
        }
    }

    fn reply(&self, account: &str, response: StreamResponse) {
        self.script
            .lock()
            .unwrap()
            .replies
            .entry(account.to_owned())
            .or_default()
            .push_back(response);
    }

    fn send(&self, key: &str) -> Result<StreamResponse, AgentError> {
        let key = CacheKey::task(None, key);
        smol::block_on(self.router.stream_message(
            &self.model,
            &[],
            "",
            &Value::Null,
            &self.events_tx,
            RequestOptions::default(),
            Some(&key),
        ))
    }

    fn failover(&self, key: &str, error: &AgentError) -> Failover {
        smol::block_on(
            self.router
                .failover(error, Some(&CacheKey::task(None, key))),
        )
    }

    fn served(&self) -> Vec<String> {
        self.script.lock().unwrap().served.clone()
    }

    fn last_served(&self) -> String {
        self.served().last().cloned().unwrap_or_default()
    }

    fn switches(&self) -> Vec<Option<AccountSwitch>> {
        self.events
            .try_iter()
            .filter_map(|event| match event {
                ProviderEvent::AccountRouted { switch, .. } => Some(switch),
                _ => None,
            })
            .collect()
    }
}

fn subscription(name: &str) -> AccountCredential {
    AccountCredential {
        id: AccountId::new(SLUG, AccountName::new(name).unwrap()),
        secret: AccountSecret::OAuth(OAuthTokens {
            access: name.to_owned(),
            refresh: name.to_owned(),
            expires: u64::MAX,
            account_id: None,
        }),
        source: CredentialSource::Stored,
        identity: AccountIdentity::default(),
    }
}

fn pinned(name: &str) -> AccountSelection {
    AccountSelection::Pinned(AccountName::new(name).unwrap())
}

fn spent_reply() -> StreamResponse {
    StreamResponse {
        quota: vec![QuotaWindow::new(
            WindowKind::FiveHour,
            SPENT_PERCENT,
            Some(now_millis() + HOUR_MS),
        )],
        ..StreamResponse::default()
    }
}

fn refusal(status: u16, retry_after: Option<Duration>) -> AgentError {
    AgentError::Api {
        status,
        message: REFUSED.into(),
        retry_after,
    }
}

#[test]
fn a_conversation_stays_on_its_account() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    harness.send(SESSION).unwrap();
    harness.send(SESSION).unwrap();
    assert_eq!(harness.served(), vec![PRIMARY, PRIMARY]);
    assert_eq!(harness.switches(), vec![None]);
}

#[test]
fn responses_name_the_account_that_served_them() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    let account = harness.send(SESSION).unwrap().account.unwrap();
    assert_eq!(
        (account.name.as_str(), account.mode, account.pool_size),
        (PRIMARY, SelectionMode::Auto, 2)
    );
}

#[test]
fn quota_headers_move_a_spent_conversation() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    harness.reply(PRIMARY, spent_reply());
    harness.send(SESSION).unwrap();
    harness.send(SESSION).unwrap();
    assert_eq!(harness.served(), vec![PRIMARY, SECONDARY]);
    assert_eq!(
        harness.switches(),
        vec![
            None,
            None,
            Some(AccountSwitch {
                from: PRIMARY.into(),
                reason: SwitchReason::Spent {
                    window: WindowKind::FiveHour,
                    used_percent: SPENT_PERCENT as u8,
                },
                cached_tokens: 0,
            }),
        ]
    );
}

#[test_case(LONG_WAIT, Failover::Switched, SECONDARY ; "long_wait_switches")]
#[test_case(SHORT_WAIT, Failover::STAY, PRIMARY ; "short_wait_stays")]
fn rate_limits_switch_only_when_the_wait_is_long(wait: Duration, expected: Failover, next: &str) {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    harness.send(SESSION).unwrap();
    let failover = harness.failover(SESSION, &refusal(STATUS_RATE_LIMITED, Some(wait)));
    harness.send(SESSION).unwrap();
    assert_eq!(
        (failover, harness.last_served()),
        (expected, next.to_owned())
    );
}

#[test]
fn a_failover_is_reported_with_the_retry() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    harness.send(SESSION).unwrap();
    harness.failover(SESSION, &refusal(STATUS_UNAUTHORIZED, None));
    harness.send(SESSION).unwrap();
    let switch = harness.switches().pop().flatten().unwrap();
    assert_eq!(
        (switch.from.as_str(), switch.reason),
        (PRIMARY, SwitchReason::AuthFailed)
    );
}

#[test_case(AccountSelection::Auto, Failover::Switched, SECONDARY ; "auto_moves")]
#[test_case(pinned(PRIMARY), Failover::STAY, PRIMARY ; "pinned_stays")]
fn auth_failures_move_only_in_auto(selection: AccountSelection, expected: Failover, next: &str) {
    let harness = Harness::new(&[PRIMARY, SECONDARY], selection);
    harness.send(SESSION).unwrap();
    let failover = harness.failover(SESSION, &refusal(STATUS_UNAUTHORIZED, None));
    harness.send(SESSION).unwrap();
    assert_eq!(
        (failover, harness.last_served()),
        (expected, next.to_owned())
    );
}

#[test]
fn a_lone_account_keeps_serving_after_a_refusal() {
    let harness = Harness::new(&[PRIMARY], AccountSelection::Auto);
    harness.send(SESSION).unwrap();
    let failover = harness.failover(SESSION, &refusal(STATUS_UNAUTHORIZED, None));
    harness.send(SESSION).unwrap();
    assert_eq!(
        (failover, harness.last_served()),
        (Failover::STAY, PRIMARY.to_owned())
    );
}

#[test]
fn a_spent_pinned_account_offers_the_others() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], pinned(PRIMARY));
    harness.send(SESSION).unwrap();
    let Failover::Stay {
        exhausted: Some(exhaustion),
    } = harness.failover(SESSION, &refusal(STATUS_RATE_LIMITED, Some(LONG_WAIT)))
    else {
        panic!("a long rate limit on a pinned account should report exhaustion");
    };
    let alternatives: Vec<&str> = exhaustion
        .alternatives
        .iter()
        .map(|alternative| alternative.name.as_str())
        .collect();
    assert_eq!(
        (exhaustion.account.as_str(), exhaustion.mode, alternatives),
        (PRIMARY, SelectionMode::Pinned, vec![SECONDARY])
    );
    assert!(exhaustion.until_ms.is_some());
}

#[test]
fn a_subagent_starts_on_its_sessions_account() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    let now_ms = now_millis();
    harness
        .state
        .health
        .block(PRIMARY, now_ms + HOUR_MS, BLOCKED_RATE_LIMITED, now_ms);
    harness.send(SESSION).unwrap();
    harness.state.health.served(PRIMARY, now_ms);
    harness.send(CHILD).unwrap();
    harness.send(OTHER_SESSION).unwrap();
    assert_eq!(harness.served(), vec![SECONDARY, SECONDARY, PRIMARY]);
}

#[test]
fn a_selection_moves_the_conversation_at_its_next_request() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    harness.send(SESSION).unwrap();
    harness.state.replace_snapshot(snapshot(
        vec![
            slot(subscription(PRIMARY), false),
            slot(subscription(SECONDARY), false),
        ],
        pinned(SECONDARY),
    ));
    harness.send(SESSION).unwrap();
    let switch = harness.switches().pop().flatten().unwrap();
    assert_eq!(
        (harness.last_served(), switch.reason),
        (SECONDARY.to_owned(), SwitchReason::Selected)
    );
}

#[test]
fn an_idle_conversation_turns_cold() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    harness.send(SESSION).unwrap();
    harness
        .state
        .lineages
        .lock()
        .unwrap()
        .age(SESSION, UNKNOWN_CACHE_IDLE);
    let standing = harness
        .router
        .standing(Some(SESSION), &harness.model, hash_text(""), 0);
    assert!(matches!(standing, Standing::Cold { .. }));
}

#[test]
fn list_models_asks_every_signed_in_account() {
    let harness = Harness::new(&[PRIMARY, SECONDARY], AccountSelection::Auto);
    let models = smol::block_on(harness.router.list_models()).unwrap();
    assert!(models.is_empty());
    assert_eq!(harness.state.built_instances().len(), 2);
}
