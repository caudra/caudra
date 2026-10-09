//! The background summarizer: builds the tree's lines with the Memory model.
//!
//! A runtime keeps one [`Pump`] per memory. The pump folds the view, takes
//! the nodes [`Tree::work`](super::tree::Tree::work) offers, most urgent
//! first, leases each so other runtimes on the same journal leave it alone,
//! asks the model for its line and stores it. The Memory job resolves
//! against the session's chat model, so a switch reaches the next line.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use caudra_config::ModelPolicy;
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::{Provider, from_model_async};
use caudra_providers::{
    AgentError, CacheKey, ContentBlock, MIN_THINKING_BUDGET, Message, Model, ModelError,
    ModelPurpose, RequestOptions, Role, Timeouts, TokenUsage,
};
use caudra_storage::StateDir;
use caudra_storage::memory_journal::{Entry, MemoryJournal, MemoryJournalError, StoredNode};
use caudra_storage::usage_ledger::{LedgerPurpose, TurnUsage, UsageLedger};
use flume::Sender;
use futures_lite::future::{self, Boxed};
use serde_json::json;
use smol::Task;
use thiserror::Error;
use tracing::{debug, info, warn};

use super::store::{MemoryError, MemoryState, MemoryStore, leaf_kind, now_ms};
use super::tree::{NODE, Part, VIEW, entry_text, scale};
use crate::agent::requirements::{clean, response_text};
use crate::agent::task_runner::ModelResolver;
use crate::nudge::Nudge;
use crate::types::{AgentEvent, EventSender};

/// Replies one node may take, all in one conversation, before the shortest
/// is cut to fit.
pub const TRIES: usize = 5;
/// Nodes built at once: a merge can run while a leaf is written, and the
/// user's rate limits are left for the conversation.
pub const JOBS: usize = 2;
/// How long a claim keeps other runtimes off a node.
pub const LEASE: Duration = Duration::from_secs(120);
/// How long a node that failed for a passing reason waits for its next
/// attempt.
pub const RETRY: Duration = Duration::from_secs(10);
/// A reply slower than this is a failed try, so every try of a node fits
/// inside its lease.
const CALL_TIMEOUT: Duration = Duration::from_secs(LEASE.as_secs() / TRIES as u64);
const LEASE_MS: i64 = LEASE.as_millis() as i64;
/// A line is a few hundred tokens, but a model that reasons unconditionally
/// draws its thinking budget from this same pool, and providers floor that
/// budget at [`MIN_THINKING_BUDGET`].
const OUTPUT_TOKENS: u32 = MIN_THINKING_BUDGET * 2;
const SYSTEM: &str = include_str!("../prompts/memory_compact.md");
const MEMORY_OPEN: &str = "<memory>\n";
const MEMORY_CLOSE: &str = "</memory>";
const COMPRESS: &str = "Compress this note into one line";
const MERGE: &str = "Merge these two lines into one";
const LIMIT_MARK: &str = "| ← LIMIT";
/// Separators a cut line would otherwise end on.
const TRAILING: [char; 3] = [',', ';', ':'];

/// The pump's clock and waits, behind a trait so tests decide when time
/// passes.
pub trait Timer: Send + Sync {
    /// Unix milliseconds, which leases are stamped in.
    fn now_ms(&self) -> i64;
    /// Completes once `delay` has passed.
    fn after(&self, delay: Duration) -> Boxed<()>;
}

struct SystemTimer;

impl Timer for SystemTimer {
    fn now_ms(&self) -> i64 {
        now_ms()
    }

    fn after(&self, delay: Duration) -> Boxed<()> {
        Box::pin(async move {
            smol::Timer::after(delay).await;
        })
    }
}

/// The usage ledger, for a runtime that records no spend from the events it
/// forwards.
#[derive(Clone)]
pub struct Ledger {
    pub state: StateDir,
    /// The project the spend is filed under.
    pub cwd: String,
}

/// Where each call's spend goes: an event for the runtime, and the ledger
/// when the runtime keeps none of its own.
struct Spend {
    events: EventSender,
    ledger: Option<Ledger>,
}

impl Spend {
    async fn report(&self, model: &Model, usage: TokenUsage) {
        let cost = model.billed_cost(&usage, false);
        self.events.try_send(AgentEvent::ModelUsage {
            usage,
            cost,
            billing: model.billing,
            provider: model.provider.to_string(),
            model: model.id.clone(),
            purpose: LedgerPurpose::Memory,
        });
        let Some(Ledger { state, cwd }) = self.ledger.clone() else {
            return;
        };
        let turn = TurnUsage {
            provider: model.provider.to_string(),
            model: model.id.clone(),
            cwd,
            purpose: LedgerPurpose::Memory,
            input: usage.input,
            output: usage.output,
            cache_creation: usage.cache_creation,
            cache_read: usage.cache_read,
            cost,
            subscription: model.billing.is_subscription(),
        };
        if let Err(error) = smol::unblock(move || UsageLedger::open(&state)?.record(&turn)).await {
            warn!(%error, model = %model.id, "memory summary spend not recorded in the usage ledger");
        }
    }
}

/// The model that writes the lines, and the provider that serves it.
pub struct Summarizer {
    provider: Arc<dyn Provider>,
    model: Model,
}

impl Summarizer {
    /// The Memory job resolved against the chat model, off the executor.
    /// `None`, logged, when a Memory binding does not resolve or its model
    /// will not load.
    pub async fn resolve(
        chat_provider: &Arc<dyn Provider>,
        chat_model: &Model,
        policy: &ModelPolicy,
        timeouts: Timeouts,
    ) -> Option<Self> {
        let (anchor, policy) = (chat_model.clone(), policy.clone());
        let resolved =
            smol::unblock(move || Model::resolve(ModelPurpose::Memory, &anchor, &policy)).await;
        Self::load(resolved, chat_provider, chat_model, timeouts).await
    }

    async fn load(
        resolved: Result<Model, ModelError>,
        chat_provider: &Arc<dyn Provider>,
        chat_model: &Model,
        timeouts: Timeouts,
    ) -> Option<Self> {
        let mut model = match resolved {
            Ok(model) => model,
            Err(error) => {
                warn!(
                    %error,
                    chat_model = %chat_model.spec(),
                    purpose = %ModelPurpose::Memory,
                    "memory summaries off: the memory model does not resolve"
                );
                return None;
            }
        };
        model.max_output_tokens = Some(
            model
                .max_output_tokens
                .map_or(OUTPUT_TOKENS, |cap| cap.min(OUTPUT_TOKENS)),
        );
        let provider = if model.provider == chat_model.provider {
            chat_provider.adjust_model(&mut model);
            Arc::clone(chat_provider)
        } else {
            match from_model_async(&mut model, timeouts).await {
                Ok(provider) => Arc::from(provider),
                Err(error) => {
                    warn!(
                        %error,
                        model = %model.spec(),
                        purpose = %ModelPurpose::Memory,
                        "memory summaries off: the memory model's provider does not load"
                    );
                    return None;
                }
            }
        };
        Some(Self { provider, model })
    }
}

/// Finds the summarizer for a chat model and the provider serving it.
type Resolve = Arc<dyn Fn(Arc<dyn Provider>, Model) -> Boxed<Option<Summarizer>> + Send + Sync>;

fn resolver(policy: Arc<ModelPolicy>, timeouts: Timeouts) -> Resolve {
    Arc::new(move |provider, model| {
        let policy = Arc::clone(&policy);
        Box::pin(async move { Summarizer::resolve(&provider, &model, &policy, timeouts).await })
    })
}

/// What the summarizer was chosen from: the chat model, its provider, and
/// every job binding, since the Memory job may follow another job's.
struct Seen {
    provider: Arc<dyn Provider>,
    spec: String,
    bindings: [Option<Binding>; ModelPurpose::ALL.len()],
}

impl Seen {
    fn now(provider: Arc<dyn Provider>, spec: String) -> Self {
        Self {
            provider,
            spec,
            bindings: ModelPurpose::ALL.map(model_registry::binding),
        }
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.provider, &other.provider)
            && self.spec == other.spec
            && self.bindings == other.bindings
    }
}

/// The summarizer last chosen, looked up again only when the session
/// switches chat model or provider, or a job binding changes. Lines in
/// progress keep the summarizer they started with.
#[derive(Default)]
struct Choice {
    seen: Option<Seen>,
    summarizer: Option<Arc<Summarizer>>,
}

impl Choice {
    /// True when the chat model or a binding changed since the last look,
    /// and with it, perhaps, the summarizer.
    async fn follow(&mut self, chat: &ModelResolver, resolve: &Resolve) -> bool {
        let (provider, model) = chat();
        let spec = model.spec();
        let seen = Seen::now(Arc::clone(&provider), spec.clone());
        if self.seen.as_ref().is_some_and(|last| last.same(&seen)) {
            return false;
        }
        self.summarizer = resolve(Arc::clone(&provider), Model::clone(&model))
            .await
            .map(Arc::new);
        if let Some(summarizer) = &self.summarizer {
            info!(
                chat_model = %spec,
                model = %summarizer.model.spec(),
                purpose = %ModelPurpose::Memory,
                "memory summaries follow the chat model"
            );
        }
        self.seen = Some(seen);
        true
    }
}

/// Builds one memory's lines in the background until dropped.
pub struct Pump {
    store: Arc<MemoryStore>,
    nudge: Nudge,
    _task: Task<()>,
}

impl Pump {
    /// Builds every node the view needs, each with the summarizer for the
    /// chat model `chat` names when the node starts. Waits, logged, while
    /// that chat model has no summarizer. `ledger` is for a runtime that
    /// records no spend from `events`.
    pub fn start(
        store: Arc<MemoryStore>,
        chat: ModelResolver,
        policy: Arc<ModelPolicy>,
        timeouts: Timeouts,
        events: EventSender,
        ledger: Option<Ledger>,
    ) -> Self {
        let nudge = Nudge::default();
        let shared = Shared::new(
            Arc::clone(&store),
            chat,
            resolver(policy, timeouts),
            Spend { events, ledger },
            Arc::new(SystemTimer),
        );
        let task = smol::spawn(Arc::new(shared).run(nudge.clone()));
        Self {
            store,
            nudge,
            _task: task,
        }
    }

    pub fn store(&self) -> &Arc<MemoryStore> {
        &self.store
    }

    /// Wakes the pump if it waits for work: entries may have arrived, and
    /// nodes that failed for good get another attempt.
    pub fn nudge(&self) {
        self.nudge.notify();
    }
}

/// Why a node was not built.
#[derive(Debug, Error)]
enum BuildError {
    #[error(transparent)]
    Model(#[from] AgentError),
    #[error("the model answered with nothing")]
    Empty,
    #[error("entry {0} is gone from the journal")]
    Gone(u64),
    #[error(transparent)]
    Memory(#[from] MemoryError),
}

impl BuildError {
    /// Whether waiting may help: the provider was busy, limiting or out of
    /// reach.
    fn transient(&self) -> bool {
        matches!(self, Self::Model(error) if error.is_retryable())
    }
}

/// What a node is made from.
enum Material {
    /// The entry at this seq, compressed alone. Its body is read when the
    /// build starts.
    Entry(u64),
    /// Two adjacent lines, merged.
    Lines(String, String),
}

/// A node about to be built, with what the model reads for it and the
/// summarizer that writes it.
struct Job {
    part: Part,
    memory: String,
    material: Material,
    summarizer: Arc<Summarizer>,
}

impl Job {
    fn new(
        state: &MemoryState,
        parts: &[Part],
        part: Part,
        summarizer: &Arc<Summarizer>,
    ) -> Option<Self> {
        let tree = &state.tree;
        let material = match part.children() {
            None => Material::Entry(part.index),
            Some([first, second]) => Material::Lines(
                tree.built(&first)?.text.clone(),
                tree.built(&second)?.text.clone(),
            ),
        };
        Some(Self {
            memory: memory_block(&tree.context(parts, &part)),
            part,
            material,
            summarizer: Arc::clone(summarizer),
        })
    }
}

/// Why the pump woke.
enum Wake {
    Built(Part, Result<(), BuildError>),
    /// A node that failed for a passing reason may be tried again.
    Due(Part),
    /// A lease another runtime held on a needed node ran out.
    Lapsed,
}

/// What a pump's builds share.
struct Shared {
    store: Arc<MemoryStore>,
    chat: ModelResolver,
    resolve: Resolve,
    spend: Spend,
    timer: Arc<dyn Timer>,
    /// Unique to this pump, so two pumps never build one node, even in one
    /// process.
    owner: String,
    key: CacheKey,
}

impl Shared {
    fn new(
        store: Arc<MemoryStore>,
        chat: ModelResolver,
        resolve: Resolve,
        spend: Spend,
        timer: Arc<dyn Timer>,
    ) -> Self {
        Self {
            key: CacheKey::memory(store.scope()),
            owner: format!("{}-{:016x}", std::process::id(), fastrand::u64(..)),
            store,
            chat,
            resolve,
            spend,
            timer,
        }
    }

    async fn run(self: Arc<Self>, nudge: Nudge) {
        let (wake_tx, wake_rx) = flume::unbounded();
        let mut building: HashMap<Part, Task<()>> = HashMap::new();
        let mut cooling: HashMap<Part, Task<()>> = HashMap::new();
        let mut held: HashSet<Part> = HashSet::new();
        let mut warned: HashSet<Part> = HashSet::new();
        let mut choice = Choice::default();
        let mut lapse: Option<Task<()>> = None;
        // Kept across wakes, so a nudge that lands while one is handled waits
        // its turn instead of being lost.
        let mut nudged = nudge.listen();
        loop {
            if choice.follow(&self.chat, &self.resolve).await {
                held.clear();
            }
            if let Some(summarizer) = &choice.summarizer {
                match self.load().await {
                    Ok(state) => {
                        let lapses = self.start(
                            &state,
                            summarizer,
                            &mut building,
                            |part| cooling.contains_key(part) || held.contains(part),
                            &wake_tx,
                        );
                        lapse = lapses.map(|expires| self.lapse(expires, &wake_tx));
                    }
                    Err(error) => {
                        warn!(scope = self.store.scope(), %error, "memory not loaded for summaries")
                    }
                }
            }
            let woke = future::or(async { wake_rx.recv_async().await.ok() }, async {
                (&mut nudged).await;
                None
            })
            .await;
            match woke {
                Some(Wake::Built(part, result)) => {
                    building.remove(&part);
                    let Err(error) = result else {
                        continue;
                    };
                    let transient = error.transient();
                    if warned.insert(part.clone()) {
                        warn!(scope = self.store.scope(), node = %part, %error, transient, "memory line not written");
                    } else {
                        debug!(scope = self.store.scope(), node = %part, %error, transient, "memory line not written again");
                    }
                    if transient {
                        let task = self.cool(part.clone(), &wake_tx);
                        cooling.insert(part, task);
                    } else {
                        held.insert(part);
                    }
                }
                Some(Wake::Due(part)) => {
                    cooling.remove(&part);
                }
                Some(Wake::Lapsed) => {
                    lapse.take();
                }
                None => {
                    nudged = nudge.listen();
                    held.clear();
                }
            }
        }
    }

    /// Starts the nodes the view needs, most urgent first, while fewer than
    /// [`JOBS`] build. Skips nodes another pump holds and those `waiting`
    /// keeps back. Returns when the first lease it skipped runs out.
    fn start(
        self: &Arc<Self>,
        state: &MemoryState,
        summarizer: &Arc<Summarizer>,
        building: &mut HashMap<Part, Task<()>>,
        waiting: impl Fn(&Part) -> bool,
        wake: &Sender<Wake>,
    ) -> Option<i64> {
        let parts = state.tree.fold(VIEW);
        let now_ms = self.timer.now_ms();
        let mut lapses: Option<i64> = None;
        for part in state.tree.work(&parts) {
            if building.len() >= JOBS {
                break;
            }
            if building.contains_key(&part) || waiting(&part) {
                continue;
            }
            if let Some(expires) = self.leased_elsewhere(&state.nodes, &part, now_ms) {
                lapses = Some(lapses.map_or(expires, |first| first.min(expires)));
                continue;
            }
            let Some(job) = Job::new(state, &parts, part.clone(), summarizer) else {
                continue;
            };
            let task = smol::spawn({
                let shared = Arc::clone(self);
                let wake = wake.clone();
                async move {
                    let result = shared.build(&job).await;
                    let _ = wake.send(Wake::Built(job.part, result));
                }
            });
            building.insert(part, task);
        }
        lapses
    }

    fn cool(&self, part: Part, wake: &Sender<Wake>) -> Task<()> {
        let (wait, wake) = (self.timer.after(RETRY), wake.clone());
        smol::spawn(async move {
            wait.await;
            let _ = wake.send(Wake::Due(part));
        })
    }

    /// When another runtime's live lease on the node runs out.
    fn leased_elsewhere(&self, nodes: &[StoredNode], part: &Part, now_ms: i64) -> Option<i64> {
        nodes
            .iter()
            .filter(|node| {
                node.level == part.level
                    && node.index == part.index
                    && node.text.is_none()
                    && node
                        .lease_owner
                        .as_deref()
                        .is_some_and(|owner| owner != self.owner)
            })
            .find_map(|node| node.lease_expires_ms.filter(|&expires| expires > now_ms))
    }

    /// A wake for when a lease this pump skipped runs out, since a runtime
    /// that died holding it never says so.
    fn lapse(&self, expires_ms: i64, wake: &Sender<Wake>) -> Task<()> {
        let delay = Duration::from_millis((expires_ms - self.timer.now_ms()).max(0) as u64);
        let (wait, wake) = (self.timer.after(delay), wake.clone());
        smol::spawn(async move {
            wait.await;
            let _ = wake.send(Wake::Lapsed);
        })
    }

    /// Leases the node, writes its line and stores it. A failed node is
    /// released at once, so no other runtime waits out the lease.
    async fn build(&self, job: &Job) -> Result<(), BuildError> {
        if !self.claim(&job.part).await? {
            return Ok(());
        }
        match self.write(job).await {
            Ok(stored) => {
                if stored {
                    self.spend.events.try_send(AgentEvent::MemoryChanged);
                }
                Ok(())
            }
            Err(error) => {
                if let Err(release) = self.release(&job.part).await {
                    warn!(scope = self.store.scope(), node = %job.part, error = %release, "memory lease not released");
                }
                Err(error)
            }
        }
    }

    /// False when the lease went meanwhile: the note was forgotten, or
    /// another runtime took the node over.
    async fn write(&self, job: &Job) -> Result<bool, BuildError> {
        let step = match &job.material {
            Material::Entry(seq) => {
                let entry = self.entry(*seq).await?;
                let kind = leaf_kind(&entry.meta);
                compress(&entry_text(kind, &entry.meta.name, &entry.body))
            }
            Material::Lines(first, second) => merge(first, second),
        };
        let line = self.line(&job.summarizer, &job.memory, step).await?;
        Ok(self.store_line(&job.part, &job.summarizer, line).await?)
    }

    /// Up to [`TRIES`] replies in one conversation, each one too long
    /// answered with how much of it fits. The first that fits wins, else the
    /// shortest, cut to fit.
    async fn line(
        &self,
        summarizer: &Summarizer,
        memory: &str,
        step: String,
    ) -> Result<String, BuildError> {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: memory.to_owned(),
                },
                ContentBlock::Text { text: step },
            ],
            ..Message::default()
        }];
        let mut shortest = String::new();
        for _ in 0..TRIES {
            let reply = self.ask(summarizer, &messages).await?;
            if reply.len() <= NODE {
                return Ok(reply);
            }
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: reply.clone(),
                }],
                ..Message::default()
            });
            messages.push(Message::user(feedback(&reply)));
            if shortest.is_empty() || reply.len() < shortest.len() {
                shortest = reply;
            }
        }
        Ok(cut(&shortest).to_owned())
    }

    /// One reply, trimmed. Its spend is reported whatever it says.
    async fn ask(
        &self,
        Summarizer { provider, model }: &Summarizer,
        messages: &[Message],
    ) -> Result<String, BuildError> {
        // A provider fails a request whose event channel closed.
        let (events, _events) = flume::unbounded();
        let tools = json!([]);
        let request = provider.stream_message(
            model,
            messages,
            SYSTEM,
            &tools,
            &events,
            RequestOptions::default().clamped(model),
            Some(&self.key),
        );
        let timeout = self.timer.after(CALL_TIMEOUT);
        let response = future::or(request, async {
            timeout.await;
            Err(AgentError::Timeout {
                secs: CALL_TIMEOUT.as_secs(),
            })
        })
        .await?;
        self.spend.report(model, response.usage).await;
        clean(&response_text(&response.message)).ok_or(BuildError::Empty)
    }

    async fn load(&self) -> Result<MemoryState, MemoryError> {
        let store = Arc::clone(&self.store);
        smol::unblock(move || store.load()).await
    }

    async fn journal<T: Send + 'static>(
        &self,
        query: impl FnOnce(&MemoryJournal, &str) -> Result<T, MemoryJournalError> + Send + 'static,
    ) -> Result<T, MemoryError> {
        let store = Arc::clone(&self.store);
        smol::unblock(move || Ok(query(&store.journal(), store.scope())?)).await
    }

    async fn entry(&self, seq: u64) -> Result<Entry, BuildError> {
        self.journal(move |journal, scope| journal.entry(scope, seq))
            .await?
            .ok_or(BuildError::Gone(seq))
    }

    async fn claim(&self, part: &Part) -> Result<bool, MemoryError> {
        let (level, index, owner) = (part.level, part.index, self.owner.clone());
        let now_ms = self.timer.now_ms();
        self.journal(move |journal, scope| {
            journal.claim(scope, level, index, &owner, now_ms, LEASE_MS)
        })
        .await
    }

    async fn release(&self, part: &Part) -> Result<(), MemoryError> {
        let (level, index, owner) = (part.level, part.index, self.owner.clone());
        self.journal(move |journal, scope| journal.release(scope, level, index, &owner))
            .await
    }

    async fn store_line(
        &self,
        part: &Part,
        summarizer: &Summarizer,
        line: String,
    ) -> Result<bool, MemoryError> {
        let (level, index, owner) = (part.level, part.index, self.owner.clone());
        let (model, now_ms) = (summarizer.model.spec(), self.timer.now_ms());
        self.journal(move |journal, scope| {
            journal.store_node(scope, level, index, &owner, &line, &model, now_ms)
        })
        .await
    }
}

/// The view up to the node, a line each and without addresses, so the model
/// never learns to write them.
fn memory_block(context: &[&str]) -> String {
    let mut block = String::from(MEMORY_OPEN);
    for line in context {
        block.push_str(&flat(line));
        block.push('\n');
    }
    block.push_str(MEMORY_CLOSE);
    block
}

fn compress(note: &str) -> String {
    step(&format!("{COMPRESS}, in at most {NODE} bytes"), note)
}

/// Two full lines rarely fit one, and a model left to choose keeps the
/// first whole and drops the second, so each is given its half.
fn merge(first: &str, second: &str) -> String {
    step(
        &format!(
            "{MERGE}, in at most {NODE} bytes, about {} for each",
            NODE / 2
        ),
        &format!("{}\n{}", flat(first), flat(second)),
    )
}

/// Models cannot count bytes, so the step shows the limit as a ruler.
fn step(task: &str, material: &str) -> String {
    format!("{}\n{task}:\n{material}", scale())
}

/// An overlong reply comes back with how much of it fits, framed as a
/// rewrite: asked where it had to end, models copied the cut, mid-word.
fn feedback(reply: &str) -> String {
    format!(
        "That line is {} bytes, {} over the limit of {NODE}. Rewrite all of it shorter, cutting \
         the least valuable items rather than ending it early. This much of it fits:\n{}{LIMIT_MARK}",
        reply.len(),
        reply.len() - NODE,
        fitting(reply)
    )
}

fn fitting(text: &str) -> &str {
    &text[..text.floor_char_boundary(NODE)]
}

/// The last resort for a reply that never fit: it ends after its last whole
/// word, or where the limit falls when no word ends before it.
fn cut(text: &str) -> &str {
    let fits = fitting(text);
    let whole = fits.len() == text.len() || text[fits.len()..].starts_with(char::is_whitespace);
    let words = match fits.rfind(char::is_whitespace) {
        Some(end) if !whole => &fits[..end],
        _ => fits,
    };
    words.trim_end_matches(|c: char| c.is_whitespace() || TRAILING.contains(&c))
}

fn flat(line: &str) -> String {
    line.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ops::Range;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{ModelInfo, ProviderEvent, StopReason, StreamResponse};
    use caudra_storage::memory_journal::EntryOrigin;
    use flume::Receiver;
    use serde_json::Value;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::memory::store::Source;
    use crate::types::{Envelope, MEMORY_EVENT_RUN_ID};

    const SCOPE: &str = "projects/compactor";
    const SESSION: &str = "session-a";
    const NOTES_DIR: &str = "memories";
    const NOTE_SUFFIX: &str = ".md";
    const CWD: &str = "/work/project";
    const CHAT_SPEC: &str = "openai/gpt-5.6-sol";
    const FAST_SPEC: &str = "openai/gpt-5.6-luna";
    /// Refuses a binding to the OpenAI Fast model.
    const ONLY_ANTHROPIC: &str = "anthropic/*";
    /// Four leaves, the two merges over them, and the one over those.
    const NOTES: u64 = 4;
    const NODES: usize = 7;
    /// Too long for a note to stand as its own line.
    const BODY_BYTES: usize = NODE + 64;
    /// Two of these never fit one line, so every merge asks the model.
    const LINE_BYTES: usize = NODE * 3 / 5;
    const USAGE: TokenUsage = TokenUsage {
        input: 120,
        output: 30,
        cache_creation: 0,
        cache_read: 0,
    };
    const RATE_LIMITED: u16 = 429;
    const SLOW_DOWN: &str = "slow down";
    /// One narrow character, then wide ones, so the limit splits a character.
    const NARROW: &str = "a";
    const WIDE: &str = "é";
    const WIDE_CHARS: usize = 300;
    const OVERLONG_FEEDBACK: &str = "That line is 601 bytes, 89 over the limit of 512. Rewrite all \
        of it shorter, cutting the least valuable items rather than ending it early. This much of \
        it fits:\n";
    const COMPRESS_TASK: &str = "Compress this note into one line, in at most 512 bytes:";
    const MERGE_TASK: &str =
        "Merge these two lines into one, in at most 512 bytes, about 256 for each:";
    const FIRST: &str = "flaky-tests: retry the socket test";
    const SECOND: &str = "release: tag after CI";
    const WORD: &str = "word ";
    const LISTED: &str = "word, ";
    const TAIL: &str = " tail";
    /// The last character boundary before the limit.
    const OVERLONG_CUT: usize = NODE - 1;
    /// How far past the limit each try runs.
    const OVERRUNS: [usize; TRIES] = [300, 100, 200, 400, 250];
    const SHORTEST_TRY: usize = 1;
    const UNBUILT_LINE: &str = "a node read a line no reply had written yet";
    const CONTEXT_INCOMPLETE: &str = "a node started before every line ahead of it was built";
    const BUILT_TWICE: &str = "a node was asked for more than once";
    const ONE_CONVERSATION: &str = "every try of a node is one conversation";
    const RETRIED_EARLY: &str = "a passing failure was tried again before its timer fired";
    const LEASE_KEPT: &str = "a failed node kept its lease";
    const SWITCH_MISSED: &str = "a line after a chat model switch went to the old summarizer";
    const BINDING_MISSED: &str = "a new Memory binding did not change the summarizer";
    const DEAD_OWNER: &str = "a-runtime-that-died";
    const LEASE_IGNORED: &str = "a node another runtime holds was built before its lease ran out";
    const LEASE_STALLED: &str = "a node whose lease ran out was never built";

    /// Answers with its script, then with a fresh line each request, and
    /// keeps every request with the line it answered.
    struct ScriptedProvider {
        script: Mutex<VecDeque<Result<String, AgentError>>>,
        calls: Mutex<Vec<(Vec<Message>, Option<String>)>>,
    }

    impl ScriptedProvider {
        fn new(script: impl IntoIterator<Item = Result<String, AgentError>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into_iter().collect()),
                calls: Mutex::default(),
            })
        }

        fn calls(&self) -> Vec<(Vec<Message>, Option<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let mut calls = self.calls.lock().unwrap();
                let reply = self
                    .script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok(line(calls.len())));
                calls.push((messages.to_vec(), reply.as_ref().ok().cloned()));
                reply.map(answer)
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// Time moves only when the test advances it, and a wait ends only when
    /// the test fires it.
    struct ManualTimer {
        now_ms: AtomicI64,
        waits: Mutex<Vec<(Duration, Sender<()>)>>,
        started: Sender<Duration>,
    }

    impl ManualTimer {
        fn new() -> (Arc<Self>, Receiver<Duration>) {
            let (started, waits) = flume::unbounded();
            let timer = Self {
                now_ms: AtomicI64::new(now_ms()),
                waits: Mutex::default(),
                started,
            };
            (Arc::new(timer), waits)
        }

        fn advance(&self, by: Duration) {
            self.now_ms
                .fetch_add(by.as_millis() as i64, Ordering::SeqCst);
        }

        /// Ends every wait of `delay` begun so far.
        fn fire(&self, delay: Duration) {
            self.waits.lock().unwrap().retain(|(wait, end)| {
                let due = *wait == delay;
                if due {
                    let _ = end.send(());
                }
                !due
            });
        }
    }

    impl Timer for ManualTimer {
        fn now_ms(&self) -> i64 {
            self.now_ms.load(Ordering::SeqCst)
        }

        fn after(&self, delay: Duration) -> Boxed<()> {
            let (end, ended) = flume::bounded(1);
            self.waits.lock().unwrap().push((delay, end));
            let _ = self.started.send(delay);
            Box::pin(async move {
                let _ = ended.recv_async().await;
            })
        }
    }

    struct Fixture {
        state: TempDir,
        store: Arc<MemoryStore>,
    }

    impl Fixture {
        /// A memory of `notes` notes, each too long to be its own line.
        fn new(notes: u64) -> Self {
            let state = TempDir::new().unwrap();
            let store = open(&state);
            let origin = EntryOrigin::Session(SESSION.to_owned());
            for seq in 0..notes {
                let body = format!("{seq:x>BODY_BYTES$}");
                store
                    .write(&format!("{seq}{NOTE_SUFFIX}"), &body, &origin)
                    .unwrap();
            }
            Self { state, store }
        }

        fn state_dir(&self) -> StateDir {
            StateDir::from_path(self.state.path().to_path_buf())
        }

        fn first_line(&self) -> String {
            let state = self.store.load().unwrap();
            state.tree.built(&Part::leaf(0)).unwrap().text.clone()
        }
    }

    /// A handle on the fixture's journal of its own, as another runtime has.
    fn open(state: &TempDir) -> Arc<MemoryStore> {
        let store = MemoryStore::open(
            &StateDir::from_path(state.path().to_path_buf()),
            SCOPE.to_owned(),
            Source::Local(state.path().join(NOTES_DIR)),
        );
        Arc::new(store.unwrap())
    }

    fn spawn(
        store: Arc<MemoryStore>,
        provider: &Arc<ScriptedProvider>,
        timer: &Arc<ManualTimer>,
        events: Sender<Envelope>,
        ledger: Option<Ledger>,
    ) -> (Nudge, Task<()>) {
        let chat = Arc::new(Mutex::new(Arc::clone(provider)));
        spawn_following(store, &chat, timer, events, ledger)
    }

    /// A pump whose session chats through whichever provider `chat` holds,
    /// and whose summarizer is the Fast model on that provider.
    fn spawn_following(
        store: Arc<MemoryStore>,
        chat: &Arc<Mutex<Arc<ScriptedProvider>>>,
        timer: &Arc<ManualTimer>,
        events: Sender<Envelope>,
        ledger: Option<Ledger>,
    ) -> (Nudge, Task<()>) {
        let chat: ModelResolver = Arc::new({
            let chat = Arc::clone(chat);
            move || {
                let provider: Arc<dyn Provider> = chat.lock().unwrap().clone();
                (provider, Arc::new(Model::from_spec(CHAT_SPEC).unwrap()))
            }
        });
        let resolve: Resolve = Arc::new(|provider, _| {
            Box::pin(async move {
                Some(Summarizer {
                    provider,
                    model: Model::from_spec(FAST_SPEC).unwrap(),
                })
            })
        });
        let spend = Spend {
            events: EventSender::new(events, MEMORY_EVENT_RUN_ID),
            ledger,
        };
        let shared = Shared::new(store, chat, resolve, spend, timer.clone());
        let nudge = Nudge::default();
        (nudge.clone(), smol::spawn(Arc::new(shared).run(nudge)))
    }

    /// The events up to the `count`th stored line, nudging `pumps` after each
    /// so they contend for the next node.
    async fn stored(
        events: &Receiver<Envelope>,
        count: usize,
        pumps: &[&Nudge],
    ) -> Vec<AgentEvent> {
        let mut seen = Vec::new();
        let mut stored = 0;
        while stored < count {
            match events.recv_async().await.unwrap().event {
                AgentEvent::MemoryChanged => {
                    stored += 1;
                    pumps.iter().for_each(|pump| pump.notify());
                }
                event => seen.push(event),
            }
        }
        seen
    }

    fn answer(text: String) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text }],
                ..Message::default()
            },
            usage: USAGE,
            stop_reason: Some(StopReason::EndTurn),
            ..StreamResponse::default()
        }
    }

    /// A line no other reply repeats, long enough that two never fit one.
    fn line(index: usize) -> String {
        format!("{index:0>LINE_BYTES$}")
    }

    fn texts(message: &Message) -> Vec<&str> {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The memory block and the step a node's conversation opens with.
    fn request(messages: &[Message]) -> (&str, &str) {
        let [memory, step] = texts(&messages[0])[..] else {
            panic!("a node's conversation opens with the memory block and the step");
        };
        (memory, step)
    }

    fn context(memory: &str) -> impl Iterator<Item = &str> {
        memory
            .strip_prefix(MEMORY_OPEN)
            .and_then(|rest| rest.strip_suffix(MEMORY_CLOSE))
            .unwrap()
            .lines()
    }

    /// The entries a step covers: the one a leaf compresses, or those of the
    /// two lines a merge joins.
    fn span(step: &str, covered: &HashMap<String, Range<u64>>) -> Range<u64> {
        let mut lines = step.lines().skip(1);
        let task = lines.next().unwrap();
        let first = lines.next().unwrap();
        if task.starts_with(COMPRESS) {
            let name = first.rsplit(' ').next().unwrap();
            let seq: u64 = name.strip_suffix(NOTE_SUFFIX).unwrap().parse().unwrap();
            return seq..seq + 1;
        }
        let second = lines.next().unwrap();
        let [first, second] = [first, second].map(|line| covered.get(line).expect(UNBUILT_LINE));
        first.start..second.end
    }

    /// Rule 3: leaves go in order, a merge waits for its halves, and every
    /// node reads the whole view ahead of it, so none starts before that is
    /// built.
    #[test]
    fn every_node_starts_once_the_view_ahead_of_it_is_built() {
        smol::block_on(async {
            let fixture = Fixture::new(NOTES);
            let provider = ScriptedProvider::new([]);
            let (timer, _) = ManualTimer::new();
            let (events_tx, events) = flume::unbounded();
            let _pump = spawn(
                Arc::clone(&fixture.store),
                &provider,
                &timer,
                events_tx,
                None,
            );

            stored(&events, NODES, &[]).await;

            let mut covered: HashMap<String, Range<u64>> = HashMap::new();
            for (messages, reply) in provider.calls() {
                let (memory, step) = request(&messages);
                let span = span(step, &covered);
                let mut read = 0;
                for line in context(memory) {
                    let range = covered.get(line).expect(UNBUILT_LINE);
                    assert_eq!(range.start, read, "{CONTEXT_INCOMPLETE}");
                    read = range.end;
                }
                assert_eq!(read, span.start, "{CONTEXT_INCOMPLETE}");
                covered.insert(reply.unwrap(), span);
            }
            let state = fixture.store.load().unwrap();
            assert!(state.tree.work(&state.tree.fold(VIEW)).is_empty());
        });
    }

    #[test]
    fn two_pumps_on_one_journal_build_each_node_once() {
        smol::block_on(async {
            let fixture = Fixture::new(NOTES);
            let provider = ScriptedProvider::new([]);
            let (timer, _) = ManualTimer::new();
            let (events_tx, events) = flume::unbounded();
            let first_store = Arc::clone(&fixture.store);
            let (first, _first) = spawn(first_store, &provider, &timer, events_tx.clone(), None);
            let second_store = open(&fixture.state);
            let (second, _second) = spawn(second_store, &provider, &timer, events_tx, None);

            stored(&events, NODES, &[&first, &second]).await;

            let calls = provider.calls();
            let steps: HashSet<&str> = calls
                .iter()
                .map(|(messages, _)| request(messages).1)
                .collect();
            assert_eq!((calls.len(), steps.len()), (NODES, NODES), "{BUILT_TWICE}");
        });
    }

    #[test_case(compress(FIRST), COMPRESS_TASK, &[FIRST] ; "a_leaf")]
    #[test_case(merge(FIRST, SECOND), MERGE_TASK, &[FIRST, SECOND] ; "a_merge_gives_each_line_its_half")]
    fn each_step_states_its_budget(step: String, task: &str, material: &[&str]) {
        let lines: Vec<&str> = step.lines().collect();
        assert_eq!(lines[0], scale());
        assert_eq!(lines[1], task);
        assert_eq!(lines[2..], *material);
    }

    #[test_case(WORD.repeat(NODE), WORD.repeat(NODE / WORD.len()).trim_end().to_owned() ; "after_the_last_whole_word")]
    #[test_case(LISTED.repeat(NODE), LISTED.repeat(NODE / LISTED.len()).trim_end().trim_end_matches(',').to_owned() ; "without_the_separator_before_it")]
    #[test_case(format!("{}{TAIL}", NARROW.repeat(NODE)), NARROW.repeat(NODE) ; "whole_when_the_limit_falls_between_words")]
    #[test_case(NARROW.repeat(NODE + 1), NARROW.repeat(NODE) ; "at_the_limit_when_no_word_ends_before_it")]
    fn a_reply_that_never_fit_is_cut(reply: String, line: String) {
        assert_eq!(cut(&reply), line);
    }

    #[test]
    fn an_overlong_reply_is_shown_how_much_of_it_fits() {
        smol::block_on(async {
            let fixture = Fixture::new(1);
            let overlong = format!("{NARROW}{}", WIDE.repeat(WIDE_CHARS));
            let provider = ScriptedProvider::new([Ok(overlong.clone())]);
            let (timer, _) = ManualTimer::new();
            let (events_tx, events) = flume::unbounded();
            let _pump = spawn(
                Arc::clone(&fixture.store),
                &provider,
                &timer,
                events_tx,
                None,
            );

            stored(&events, 1, &[]).await;

            let calls = provider.calls();
            let (retry, reply) = &calls[1];
            let feedback = format!(
                "{OVERLONG_FEEDBACK}{}{LIMIT_MARK}",
                &overlong[..OVERLONG_CUT]
            );
            assert_eq!(texts(&retry[1]), [overlong.as_str()]);
            assert_eq!(texts(&retry[2]), [feedback.as_str()]);
            assert_eq!(Some(fixture.first_line()), *reply);
        });
    }

    #[test]
    fn the_shortest_try_is_cut_to_fit_when_none_fits() {
        smol::block_on(async {
            let fixture = Fixture::new(1);
            let tries: Vec<String> = OVERRUNS
                .iter()
                .zip('a'..)
                .map(|(overrun, fill)| fill.to_string().repeat(NODE + overrun))
                .collect();
            let provider = ScriptedProvider::new(tries.iter().cloned().map(Ok));
            let (timer, _) = ManualTimer::new();
            let (events_tx, events) = flume::unbounded();
            let _pump = spawn(
                Arc::clone(&fixture.store),
                &provider,
                &timer,
                events_tx,
                None,
            );

            stored(&events, 1, &[]).await;

            let calls = provider.calls();
            assert_eq!(calls.len(), TRIES);
            assert_eq!(
                calls[TRIES - 1].0.len(),
                2 * TRIES - 1,
                "{ONE_CONVERSATION}"
            );
            assert_eq!(fixture.first_line(), tries[SHORTEST_TRY][..NODE]);
        });
    }

    #[test]
    fn a_passing_failure_is_tried_again_once_its_timer_fires() {
        smol::block_on(async {
            let fixture = Fixture::new(1);
            let provider = ScriptedProvider::new([Err(AgentError::api(RATE_LIMITED, SLOW_DOWN))]);
            let (timer, waits) = ManualTimer::new();
            let (events_tx, events) = flume::unbounded();
            let _pump = spawn(
                Arc::clone(&fixture.store),
                &provider,
                &timer,
                events_tx,
                None,
            );

            while waits.recv_async().await.unwrap() != RETRY {}
            assert_eq!(provider.calls().len(), 1, "{RETRIED_EARLY}");
            assert!(
                fixture.store.load().unwrap().nodes.is_empty(),
                "{LEASE_KEPT}"
            );

            timer.fire(RETRY);
            stored(&events, 1, &[]).await;
            assert_eq!(provider.calls().len(), 2);
        });
    }

    #[test]
    fn every_call_reports_its_spend_as_memory_work() {
        smol::block_on(async {
            let fixture = Fixture::new(1);
            let provider = ScriptedProvider::new([Ok(NARROW.repeat(NODE + 1))]);
            let (timer, _) = ManualTimer::new();
            let (events_tx, events) = flume::unbounded();
            let ledger = Ledger {
                state: fixture.state_dir(),
                cwd: CWD.to_owned(),
            };
            let store = Arc::clone(&fixture.store);
            let _pump = spawn(store, &provider, &timer, events_tx, Some(ledger));

            let seen = stored(&events, 1, &[]).await;

            let fast = Model::from_spec(FAST_SPEC).unwrap();
            let spent: Vec<_> = seen
                .into_iter()
                .filter_map(|event| match event {
                    AgentEvent::ModelUsage {
                        usage,
                        provider,
                        model,
                        purpose,
                        ..
                    } => Some((usage, provider, model, purpose)),
                    _ => None,
                })
                .collect();
            let call = (
                USAGE,
                fast.provider.to_string(),
                fast.id,
                LedgerPurpose::Memory,
            );
            assert_eq!(spent, [call.clone(), call]);
            let rows = UsageLedger::open(&fixture.state_dir())
                .unwrap()
                .buckets(None)
                .unwrap();
            let recorded: u64 = rows
                .iter()
                .filter(|row| row.purpose == LedgerPurpose::Memory.storage_name() && row.cwd == CWD)
                .map(|row| row.input)
                .sum();
            assert_eq!(recorded, 2 * u64::from(USAGE.input));
        });
    }

    /// Unbound, the Memory job takes the chat model, as compaction does; a
    /// binding replaces it, and one the policy refuses pauses summaries.
    #[test_case(None, &[], Some(CHAT_SPEC) ; "unbound_takes_the_chat_model")]
    #[test_case(Some(FAST_SPEC), &[], Some(FAST_SPEC) ; "a_binding_replaces_it")]
    #[test_case(Some(FAST_SPEC), &[ONLY_ANTHROPIC], None ; "a_refused_binding_pauses")]
    fn the_memory_job_picks_the_summarizer(
        bound: Option<&str>,
        allowed: &[&str],
        expected: Option<&str>,
    ) {
        smol::block_on(async {
            let state = TempDir::new().unwrap();
            let state = StateDir::from_path(state.path().to_path_buf());
            if let Some(spec) = bound {
                let binding = Binding::Exact(spec.to_owned());
                model_registry::set_binding_and_persist(ModelPurpose::Memory, binding, &state)
                    .unwrap();
            }
            let provider: Arc<dyn Provider> = ScriptedProvider::new([]);
            let chat: ModelResolver = Arc::new(move || {
                let model = Model::from_spec(CHAT_SPEC).unwrap();
                (Arc::clone(&provider), Arc::new(model))
            });
            let allowed: Vec<String> = allowed.iter().map(|spec| (*spec).to_owned()).collect();
            let policy = ModelPolicy::new(&allowed, &[]).unwrap();
            let resolve = resolver(Arc::new(policy), Timeouts::default());
            let mut choice = Choice::default();

            choice.follow(&chat, &resolve).await;

            if bound.is_some() {
                model_registry::clear_binding_and_persist(ModelPurpose::Memory, &state).unwrap();
            }
            let chosen = choice.summarizer.map(|summarizer| summarizer.model.spec());
            assert_eq!(chosen.as_deref(), expected);
        });
    }

    /// A runtime that dies holding a lease never releases it, so a pump that
    /// skipped the node wakes when the lease runs out and builds it.
    #[test]
    fn a_lease_left_by_a_dead_runtime_is_taken_once_it_runs_out() {
        smol::block_on(async {
            let fixture = Fixture::new(1);
            let (timer, waits) = ManualTimer::new();
            let claimed = fixture
                .store
                .journal()
                .claim(SCOPE, 0, 0, DEAD_OWNER, timer.now_ms(), LEASE_MS)
                .unwrap();
            let provider = ScriptedProvider::new([]);
            let (events_tx, events) = flume::unbounded();
            let store = Arc::clone(&fixture.store);
            let _pump = spawn(store, &provider, &timer, events_tx, None);
            while waits.recv_async().await.unwrap() != LEASE {}
            assert!(claimed);
            assert!(provider.calls().is_empty(), "{LEASE_IGNORED}");

            timer.advance(LEASE);
            timer.fire(LEASE);
            stored(&events, 1, &[]).await;

            assert_eq!(provider.calls().len(), 1, "{LEASE_STALLED}");
        });
    }

    /// Binding the Memory job mid-session changes the summarizer, with the
    /// chat model left as it was.
    #[test]
    fn a_new_binding_is_followed_without_a_chat_switch() {
        smol::block_on(async {
            let state = TempDir::new().unwrap();
            let state = StateDir::from_path(state.path().to_path_buf());
            let provider: Arc<dyn Provider> = ScriptedProvider::new([]);
            let chat: ModelResolver = Arc::new(move || {
                let model = Model::from_spec(CHAT_SPEC).unwrap();
                (Arc::clone(&provider), Arc::new(model))
            });
            let resolve = resolver(Arc::new(ModelPolicy::default()), Timeouts::default());
            let mut choice = Choice::default();
            choice.follow(&chat, &resolve).await;

            let binding = Binding::Exact(FAST_SPEC.to_owned());
            model_registry::set_binding_and_persist(ModelPurpose::Memory, binding, &state).unwrap();
            let looked = choice.follow(&chat, &resolve).await;
            model_registry::clear_binding_and_persist(ModelPurpose::Memory, &state).unwrap();

            let chosen = choice.summarizer.map(|summarizer| summarizer.model.spec());
            assert!(looked, "{BINDING_MISSED}");
            assert_eq!(chosen.as_deref(), Some(FAST_SPEC), "{BINDING_MISSED}");
        });
    }

    /// A switch of chat model reaches the next attempt at a node, and the old
    /// summarizer is asked nothing more. The node waits out a rate limit, so
    /// the switch lands while nothing is being built.
    #[test]
    fn the_next_line_follows_a_switched_chat_model() {
        smol::block_on(async {
            let fixture = Fixture::new(1);
            let before = ScriptedProvider::new([Err(AgentError::api(RATE_LIMITED, SLOW_DOWN))]);
            let after = ScriptedProvider::new([]);
            let chat = Arc::new(Mutex::new(Arc::clone(&before)));
            let (timer, waits) = ManualTimer::new();
            let (events_tx, events) = flume::unbounded();
            let store = Arc::clone(&fixture.store);
            let _pump = spawn_following(store, &chat, &timer, events_tx, None);
            while waits.recv_async().await.unwrap() != RETRY {}

            *chat.lock().unwrap() = Arc::clone(&after);
            timer.fire(RETRY);
            stored(&events, 1, &[]).await;

            let calls = (before.calls().len(), after.calls().len());
            assert_eq!(calls, (1, 1), "{SWITCH_MISSED}");
        });
    }
}
