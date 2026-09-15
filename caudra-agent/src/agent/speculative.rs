use std::future::{Future, pending};
use std::sync::{Arc, Mutex, MutexGuard};

use async_lock::OnceCell;
use futures_lite::FutureExt;
use serde_json::Value;

use super::tool_dispatch::{self, Emit, RecentCalls, ResponseObservations};
use crate::AgentEvent;
use crate::cancel::CancelTrigger;
use crate::mcp::McpSession;
use crate::permissions::canonical_json;
use crate::tools::json_repair::RepairState;
use crate::tools::native::batch;
use crate::tools::{ToolContext, ToolEffect, ToolLive};
use crate::types::{BatchToolEntry, BatchToolStatus, ToolDoneEvent, ToolOutput, ToolStartEvent};
use caudra_providers::{ContentBlock, Message, Role, StreamResponse, ToolNameAliases};

const PANICKED: &str = "internal error: tool panicked";
const CANCELLED_EFFECTS: &str = "Tool execution was cancelled. Effects applied before cancellation may remain; do not blindly repeat this call.";
const NOTICE_INTRO: &str = "These calls started while your last message was still being written and have already run, so their effects are in place. Do not repeat them:";
const NOTICE_OK: &str = "ok";
const NOTICE_ERROR: &str = "error";
const NOTICE_ELLIPSIS: &str = "…";
const SUMMARY_CAP: usize = 120;
pub(crate) const REVISED_INPUT: &str = "The provider revised this call after admission. The result below belongs to the originally executed arguments; the revised call was not executed.";

/// Every child this response started before the message carrying it was whole.
pub struct SpeculativeRuns {
    /// The turn's context, minus anything a child must not inherit.
    ctx: ToolContext,
    runs: Mutex<Vec<Run>>,
    tops: Mutex<Vec<Top>>,
    aliases: Mutex<Option<ToolNameAliases>>,
    recent: Arc<Mutex<RecentCalls>>,
}

struct Top {
    id: String,
    name: String,
    ordinal: usize,
    input: Option<Value>,
    admitted: bool,
}

struct Run {
    id: String,
    parent: Option<String>,
    params: Value,
    observations: Option<ResponseObservations>,
    /// The name as the model wrote it, so a claim matches the element the
    /// batch parsed rather than the tool the registry resolved it to.
    tool: String,
    /// Enough of the arguments to tell two calls to one tool apart, kept
    /// because a call that never published a header has nothing else to be
    /// named by in the report.
    request: String,
    start: Arc<Mutex<Option<ToolStartEvent>>>,
    result: Arc<OnceCell<ToolDoneEvent>>,
    /// Dropping either of these stops the call: the token fires on drop and a
    /// dropped task is a cancelled one.
    cancel: CancelTrigger,
    task: smol::Task<()>,
}

impl Run {
    fn finished(&self) -> bool {
        self.result.get().is_some()
    }

    fn started(&self) -> Option<ToolStartEvent> {
        self.start
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// What the report calls this run: the header it introduced itself with,
    /// or its arguments when that header said no more than the name already
    /// does.
    fn named(&self) -> String {
        self.started()
            .map(|start| start.summary)
            .filter(|summary| !summary.is_empty() && *summary != self.tool)
            .unwrap_or_else(|| self.request.clone())
    }
}

/// A run the batch took over. Holding it keeps the call alive; dropping it
/// without finishing cancels it.
pub struct Adopted {
    start: Option<ToolStartEvent>,
    result: Arc<OnceCell<ToolDoneEvent>>,
    _cancel: CancelTrigger,
    _task: smol::Task<()>,
    revised: Option<String>,
}

impl Adopted {
    /// The header the call introduced itself with, once it has.
    pub fn start(&self) -> Option<&ToolStartEvent> {
        self.start.as_ref()
    }

    pub async fn finish(self) -> ToolDoneEvent {
        let mut done = self.result.wait().await.clone();
        if let Some(executed) = self.revised {
            append_report(
                &mut done,
                &format!("{REVISED_INPUT}\nExecuted request excerpt: {executed}"),
            );
            done.is_error = true;
        }
        done
    }
}

fn append_report(done: &mut ToolDoneEvent, notice: &str) {
    done.model_output = Some(format!("{}\n\n{notice}", done.composed_model_output()));
    done.model_suffix = None;
    done.output = ToolOutput::Plain(format!("{}\n\n{notice}", done.output.as_text()).into());
}

pub(crate) async fn with_live<T, F: Future<Output = T>>(
    ctx: &ToolContext,
    work: impl FnOnce(ToolContext) -> F,
) -> T {
    let (sink, live) = flume::unbounded();
    let mut live_ctx = ctx.clone();
    live_ctx.live_sink = Some(sink);
    let forwarding = async {
        while let Ok(update) = live.recv_async().await {
            publish_live(ctx, update);
        }
        pending().await
    };
    let result = work(live_ctx).or(forwarding).await;
    for update in live.drain() {
        publish_live(ctx, update);
    }
    result
}

fn publish_live(ctx: &ToolContext, update: ToolLive) {
    let id = ctx.tool_use_id.clone().unwrap_or_default();
    let event = match update {
        ToolLive::Buf(body) => AgentEvent::LiveToolBuf { id, body },
        ToolLive::Annotation(annotation) | ToolLive::Usage(annotation) => {
            AgentEvent::ToolAnnotation { id, annotation }
        }
        ToolLive::Progress(progress) => AgentEvent::SubagentProgress { progress },
    };
    ctx.event_tx.try_send(event);
}

/// What a child's row should show for a run already under way.
pub struct Peeked {
    pub start: Option<ToolStartEvent>,
    pub done: Option<ToolDoneEvent>,
}

impl Peeked {
    /// The roster row this run has earned so far, `None` while it has nothing
    /// to say that the pending row does not already.
    pub fn entry(&self) -> Option<BatchToolEntry> {
        let mut entry = if let Some(start) = &self.start {
            batch::started_entry(start)
        } else {
            BatchToolEntry {
                model_suffix: None,
                tool: self.done.as_ref()?.tool.to_string(),
                effect: ToolEffect::Unknown,
                summary: String::new(),
                status: BatchToolStatus::Pending,
                input: None,
                raw_input: None,
                output: None,
                annotation: None,
            }
        };
        if let Some(done) = &self.done {
            batch::settle_entry(&mut entry, done);
        }
        Some(entry)
    }
}

impl SpeculativeRuns {
    /// `ctx` is the turn's own context. The store is reachable from it, so the
    /// handle is stripped here rather than kept and cleared per child: a child
    /// that could start children of its own would keep the store alive
    /// forever through its own context.
    pub fn new(ctx: &ToolContext, mcp: Option<McpSession>) -> Self {
        let mut ctx = ctx.clone();
        ctx.speculative = None;
        ctx.mcp = mcp;
        ctx.steering_order = Vec::new();
        ctx.tool_name_aliases = None;
        Self {
            ctx,
            runs: Mutex::new(Vec::new()),
            tops: Mutex::new(Vec::new()),
            aliases: Mutex::new(None),
            recent: Arc::new(Mutex::new(RecentCalls::with_threshold(0))),
        }
    }

    pub(super) fn with_recent(mut self, recent: RecentCalls) -> Self {
        self.recent = Arc::new(Mutex::new(recent));
        self
    }

    pub(super) fn recent(&self) -> RecentCalls {
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(super) fn observations(&self) -> Option<ResponseObservations> {
        self.ctx.steering_observations.clone()
    }

    pub(super) fn repair_state(&self) -> Arc<RepairState> {
        Arc::clone(&self.ctx.json_repair)
    }

    pub(super) fn set_aliases(&self, aliases: Option<ToolNameAliases>) {
        *self.aliases.lock().unwrap_or_else(|e| e.into_inner()) = aliases;
    }

    pub(super) fn aliases(&self) -> Option<ToolNameAliases> {
        self.aliases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(super) fn begin_attempt(&self) {
        if !self.has_admitted() {
            self.tops.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.set_aliases(None);
        }
    }

    pub(super) fn reconcile(&self, message: &mut Message) {
        let tops = self.tops.lock().unwrap_or_else(|e| e.into_inner());
        let runs = self.lock();
        for top in tops.iter() {
            if !message.tool_uses().any(|(id, _, _)| id == top.id) {
                let input = if top.admitted {
                    top.input.clone()
                } else {
                    runs.iter()
                        .any(|run| run.parent.as_deref() == Some(&top.id))
                        .then(|| admitted_children(&runs, &top.id))
                };
                if let Some(input) = input {
                    message
                        .content
                        .push(ContentBlock::tool_use(&top.id, &top.name, input));
                }
            }
        }
    }

    pub(super) fn finalize(&self, response: &mut StreamResponse) {
        if response.tool_name_aliases.is_some() {
            self.set_aliases(response.tool_name_aliases.clone());
        }
        {
            let tops = self.tops.lock().unwrap_or_else(|e| e.into_inner());
            response
                .invalid_tool_inputs
                .retain(|id, _| !tops.iter().any(|top| top.id == *id && top.admitted));
            for (id, invalid) in &response.invalid_tool_inputs {
                self.ctx.json_repair.register_invalid(id, invalid.clone());
            }
        }
        for (id, name, input) in response.message.tool_uses() {
            self.ready(id, name, input.clone());
        }
    }

    pub(crate) fn register(&self, id: &str, name: &str) {
        self.register_source(id, name, None);
    }

    pub(super) fn register_source(&self, id: &str, name: &str, ordinal: Option<usize>) {
        if id.is_empty() || name.is_empty() {
            return;
        }
        let mut tops = self.tops.lock().unwrap_or_else(|e| e.into_inner());
        if !tops.iter().any(|top| top.id == id) {
            let ordinal = ordinal
                .unwrap_or_else(|| tops.last().map_or(0, |top| top.ordinal.saturating_add(1)));
            tops.push(Top {
                id: id.into(),
                name: name.into(),
                ordinal,
                input: None,
                admitted: false,
            });
            tops.sort_by_key(|top| top.ordinal);
        }
    }

    pub(super) fn ready(&self, id: &str, name: &str, input: Value) {
        self.register(id, name);
        let mut tops = self.tops.lock().unwrap_or_else(|e| e.into_inner());
        let Some(top) = tops.iter_mut().find(|top| top.id == id) else {
            return;
        };
        if !top.admitted {
            top.name = name.into();
            top.input = Some(input);
        }
        for top in tops.iter_mut() {
            if top.admitted {
                continue;
            }
            let Some(input) = &top.input else {
                continue;
            };
            let mut ctx = self.ctx.clone();
            ctx.tool_name_aliases = self
                .aliases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            ctx.tool_use_id = Some(top.id.clone());
            ctx.root_tool_use_id = ctx
                .root_tool_use_id
                .clone()
                .or_else(|| Some(top.id.clone()));
            ctx.steering_order = vec![top.ordinal];
            let name = ctx
                .resolve_tool_name_alias(super::streaming::canonical_tool_name(&top.name))
                .to_owned();
            if name == crate::tools::BATCH_TOOL_NAME
                && ctx
                    .registry
                    .get(&name)
                    .is_some_and(|entry| entry.tool.parse(input).is_ok())
                && let Some(entries) = input.get("tool_calls").and_then(Value::as_array)
            {
                for (index, entry) in entries.iter().take(batch::MAX_BATCH_SIZE).enumerate() {
                    self.start_child(top, index, entry);
                }
            }
            top.admitted = true;
            let has_children = self
                .lock()
                .iter()
                .any(|run| run.parent.as_deref() == Some(&top.id));
            if has_children {
                ctx.steering_observations = ctx
                    .steering_observations
                    .as_ref()
                    .map(|observations| observations.expanded_context(ctx.steering_order.clone()));
            } else {
                tool_dispatch::observe_context(&mut ctx, &top.name, input);
            }
            let revised_parent = has_children && name != crate::tools::BATCH_TOOL_NAME;
            let refusal = if revised_parent {
                Some(REVISED_INPUT.to_owned())
            } else if has_children
                || (name == crate::tools::BATCH_TOOL_NAME
                    && !ctx.local_tools.contains_key(&name)
                    && ctx.json_repair.invalid_input(&top.id).is_some())
            {
                None
            } else {
                self.admit(&ctx, &top.name, input)
            };
            let mut children = Self::new(&ctx, ctx.mcp.clone());
            children.recent = Arc::clone(&self.recent);
            let children = Arc::new(children);
            children.set_aliases(ctx.tool_name_aliases.clone());
            {
                let mut runs = self.lock();
                let mut at = 0;
                while at < runs.len() {
                    if runs[at].parent.as_deref() == Some(&top.id) {
                        children.lock().push(runs.remove(at));
                    } else {
                        at += 1;
                    }
                }
            }
            ctx.speculative = Some(children);
            self.launch(ctx, &top.name, input.clone(), None, refusal);
        }
    }

    pub(super) fn has_admitted(&self) -> bool {
        if !self.lock().is_empty() {
            return true;
        }
        self.tops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|top| top.admitted)
    }

    pub(super) fn partial_message(&self) -> Message {
        let tops = self.tops.lock().unwrap_or_else(|e| e.into_inner());
        Message {
            role: Role::Assistant,
            content: tops
                .iter()
                .filter(|top| top.admitted)
                .filter_map(|top| {
                    Some(ContentBlock::tool_use(
                        &top.id,
                        &top.name,
                        top.input.clone()?,
                    ))
                })
                .collect(),
            ..Message::default()
        }
    }

    pub(super) fn recover_partial(&self) -> Message {
        let parents = {
            let tops = self.tops.lock().unwrap_or_else(|e| e.into_inner());
            let runs = self.lock();
            tops.iter()
                .filter(|top| {
                    !top.admitted
                        && runs
                            .iter()
                            .any(|run| run.parent.as_deref() == Some(&top.id))
                })
                .map(|top| {
                    (
                        top.id.clone(),
                        top.name.clone(),
                        admitted_children(&runs, &top.id),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (id, name, input) in parents {
            self.ready(&id, &name, input);
        }
        self.partial_message()
    }

    pub fn observation_for(&self, id: &str) -> Option<ResponseObservations> {
        self.lock()
            .iter()
            .find(|run| run.id == id)?
            .observations
            .clone()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Run>> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn start(&self, batch_id: &str, index: usize, element: &str) {
        if batch_id.is_empty() {
            return;
        }
        let Ok(entry) = serde_json::from_str::<Value>(element) else {
            return;
        };
        let tops = self.tops.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(top) = tops.iter().find(|top| top.id == batch_id) {
            self.start_child(top, index, &entry);
        }
    }

    fn start_child(&self, top: &Top, index: usize, entry: &Value) {
        if top.admitted || index >= batch::MAX_BATCH_SIZE {
            return;
        }
        let batch_id = &top.id;
        let mut ctx = self.ctx.clone();
        ctx.tool_name_aliases = self
            .aliases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if ctx.resolve_tool_name_alias(super::streaming::canonical_tool_name(&top.name))
            != crate::tools::BATCH_TOOL_NAME
            || ctx.local_tools.contains_key(crate::tools::BATCH_TOOL_NAME)
            || !ctx.tool_filter.matches(crate::tools::BATCH_TOOL_NAME)
            || !ctx
                .registry
                .get(crate::tools::BATCH_TOOL_NAME)
                .is_some_and(|entry| entry.tool.audience().contains(ctx.audience))
        {
            return;
        }
        let Some((tool, params)) = batch::dispatchable(entry, &ctx) else {
            return;
        };
        ctx.tool_use_id = Some(batch::child_tool_use_id(Some(batch_id), index));
        ctx.root_tool_use_id = ctx
            .root_tool_use_id
            .clone()
            .or_else(|| Some(batch_id.to_owned()));
        ctx.steering_order = vec![top.ordinal, index];
        if self
            .lock()
            .iter()
            .any(|run| Some(&run.id) == ctx.tool_use_id.as_ref())
        {
            return;
        }
        tool_dispatch::observe_context(&mut ctx, &tool, &params);
        let refusal = self.admit(&ctx, &tool, &params);
        self.launch(
            ctx,
            &tool,
            params,
            Some((batch_id.to_owned(), index)),
            refusal,
        );
    }

    pub(crate) fn admit(&self, ctx: &ToolContext, tool: &str, input: &Value) -> Option<String> {
        let name = ctx.resolve_tool_name_alias(super::streaming::canonical_tool_name(tool));
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let repeated = recent.may_repeat_name(name) && recent.is_doom_loop(name, input);
        recent.record(name.to_owned(), input);
        if !repeated {
            return None;
        }
        ctx.mark_tool_result_repairable();
        let policy = ctx.config.steering.resolve(&ctx.model.spec());
        let guidance = policy
            .rules
            .repeated_tool_call
            .prompt
            .as_deref()
            .unwrap_or(crate::tools::DOOM_LOOP_GUIDANCE);
        Some(format!(
            "You have called this tool with identical input {} times in a row. {guidance}",
            recent.threshold()
        ))
    }

    fn launch(
        &self,
        mut ctx: ToolContext,
        tool: &str,
        params: Value,
        child: Option<(String, usize)>,
        refusal: Option<String>,
    ) {
        let (cancel, token) = ctx.cancel.child();
        ctx.cancel = token;
        let request = shortened(&canonical_json(&params));
        let id = ctx.tool_use_id.clone().unwrap_or_default();
        let observations = ctx.steering_observations.clone();
        let start = Arc::new(Mutex::new(None));
        let result = Arc::new(OnceCell::new());
        let task = smol::spawn(run_child(
            ctx,
            tool.to_owned(),
            params.clone(),
            child.as_ref().map(|(_, index)| *index),
            refusal,
            Arc::clone(&start),
            Arc::clone(&result),
        ));
        self.lock().push(Run {
            id,
            parent: child.map(|(parent, _)| parent),
            params,
            observations,
            tool: tool.to_owned(),
            request,
            start,
            result,
            cancel,
            task,
        });
    }

    /// What each of `calls` has running already, in the order asked. Answers
    /// for duplicates the way [`Self::claim`] will, so the row a batch draws
    /// before it runs is the row it keeps.
    pub fn peek_all<'a>(
        &self,
        parent: &str,
        calls: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Vec<Option<Peeked>> {
        let runs = self.lock();
        let mut taken = vec![false; runs.len()];
        calls
            .into_iter()
            .enumerate()
            .map(|(index, (tool, params))| {
                let id = batch::child_tool_use_id(Some(parent), index);
                let at = position(&runs, &taken, &id, tool, params)?;
                taken[at] = true;
                Some(Peeked {
                    start: runs[at].started(),
                    done: runs[at].result.get().cloned(),
                })
            })
            .collect()
    }

    /// Takes over the run matching this call, if there is one.
    pub fn claim(&self, id: &str, tool: &str, params: &Value) -> Option<Adopted> {
        let aliases = self.aliases();
        let canonical_name = |name: &str| {
            let name = super::streaming::canonical_tool_name(name);
            aliases
                .as_ref()
                .and_then(|aliases| aliases.get(name))
                .map_or_else(|| name.to_owned(), Clone::clone)
        };
        let mut runs = self.lock();
        let at = runs.iter().position(|run| run.id == id)?;
        let run = runs.remove(at);
        Some(Adopted {
            revised: (canonical_name(&run.tool) != canonical_name(tool)
                || canonical_json(&run.params) != canonical_json(params))
            .then(|| format!("{} {}", run.tool, run.request)),
            start: run.started(),
            result: run.result,
            _cancel: run.cancel,
            _task: run.task,
        })
    }

    /// Drops every run still going. Called when the attempt that asked for
    /// them is thrown away: their ids belong to a message that will not exist,
    /// so nothing they went on to report could be shown.
    pub fn abandon_unfinished(&self) {
        self.lock().retain(Run::finished);
    }

    /// Waits for everything started so far, so a test can assert on results
    /// rather than on timing.
    pub(crate) async fn settled(&self) {
        let cells: Vec<_> = self
            .lock()
            .iter()
            .map(|run| Arc::clone(&run.result))
            .collect();
        for cell in cells {
            cell.wait().await;
        }
    }

    /// What ran without ever being claimed, as something the model can read.
    /// Empties the store: a result kept past the response that produced it
    /// would be adopted by a later turn that deliberately asked again.
    pub fn drain_report(&self) -> Option<Message> {
        let runs = std::mem::take(&mut *self.lock());
        let mut lines = Vec::new();
        for run in &runs {
            let Some(done) = run.result.get() else {
                continue;
            };
            let outcome = if done.is_error {
                NOTICE_ERROR
            } else {
                NOTICE_OK
            };
            let named = shortened(&run.named());
            let output = shortened(&done.output.as_text());
            lines.push(format!("- {}: {named} ({outcome}): {output}", run.tool));
        }
        (!lines.is_empty())
            .then(|| Message::observation(format!("{NOTICE_INTRO}\n{}", lines.join("\n"))))
    }
}

fn admitted_children(runs: &[Run], parent: &str) -> Value {
    let mut children = Vec::new();
    for run in runs
        .iter()
        .filter(|run| run.parent.as_deref() == Some(parent))
    {
        let Some(index) = run
            .id
            .rsplit(':')
            .next()
            .and_then(|index| index.parse::<usize>().ok())
            .filter(|index| *index < batch::MAX_BATCH_SIZE)
        else {
            continue;
        };
        if children.len() <= index {
            children.resize(index + 1, Value::Null);
        }
        children[index] = serde_json::json!({ "tool": run.tool, "parameters": run.params });
    }
    serde_json::json!({ "tool_calls": children })
}

/// A header long enough to bury the list it belongs to, cut on a character
/// boundary. The notice names what ran; the transcript holds the rest.
fn shortened(summary: &str) -> String {
    let summary = summary.replace('\n', " ");
    match summary.char_indices().nth(SUMMARY_CAP) {
        Some((at, _)) => format!("{}{NOTICE_ELLIPSIS}", &summary[..at]),
        None => summary,
    }
}

/// The first run this call has not been matched to yet. An empty `taken`
/// matches nothing as taken, which is what a single claim wants.
fn position(runs: &[Run], taken: &[bool], id: &str, tool: &str, params: &Value) -> Option<usize> {
    runs.iter().enumerate().position(|(at, run)| {
        !taken.get(at).copied().unwrap_or(false)
            && run.id == id
            && run.tool == tool
            && canonical_json(&run.params) == canonical_json(params)
    })
}

/// One child, from its own dispatch to the slot the batch reads it from. The
/// row is published as it moves, so a child that started early is watched
/// early too, on the roster the stream has already drawn.
async fn run_child(
    ctx: ToolContext,
    tool: String,
    params: Value,
    index: Option<usize>,
    refusal: Option<String>,
    start: Arc<Mutex<Option<ToolStartEvent>>>,
    result: Arc<OnceCell<ToolDoneEvent>>,
) {
    let id = ctx.tool_use_id.clone().unwrap_or_default();
    let mut done = {
        let (ctx, start, tool) = (&ctx, &start, &tool);
        let call_id = id.clone();
        let call = async {
            if let Some(message) = refusal {
                if let Some(observations) = &ctx.steering_observations {
                    observations.finish(true);
                }
                return ToolDoneEvent::error(call_id.clone(), message);
            }
            with_live(ctx, |live_ctx| async move {
                let ctx = &live_ctx;
                tool_dispatch::run(
                    &ctx.registry,
                    ctx.mcp.as_ref(),
                    call_id.clone(),
                    tool,
                    &params,
                    ctx,
                    Emit::Capture(&mut |event: &ToolStartEvent| {
                        *start
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event.clone());
                        if let Some(index) = index {
                            batch::publish_child(ctx, index, batch::started_entry(event));
                        } else {
                            ctx.event_tx
                                .try_send(AgentEvent::ToolStart(Box::new(event.clone())));
                        }
                    }),
                )
                .await
            })
            .await
        };
        let call = async {
            ctx.cancel.cancelled().await;
            ToolDoneEvent::error(id.clone(), CANCELLED_EFFECTS)
        }
        .or(call);
        std::panic::AssertUnwindSafe(call)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| ToolDoneEvent::error(id, PANICKED))
    };
    if let Some(children) = &ctx.speculative {
        children.settled().await;
        if let Some(report) = children.drain_report() {
            append_report(&mut done, report.first_text_content().unwrap_or_default());
        }
    }
    done.tool =
        Arc::from(ctx.resolve_tool_name_alias(super::streaming::canonical_tool_name(&tool)));
    if let Some(index) = index {
        let peeked = Peeked {
            start: start
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            done: Some(done.clone()),
        };
        if let Some(entry) = peeked.entry() {
            batch::publish_child(&ctx, index, entry);
        }
    }
    if index.is_none() {
        ctx.event_tx
            .try_send(AgentEvent::ToolDone(Box::new(done.clone())));
    }
    let _ = result.set(done).await;
}

#[cfg(test)]
mod tests {
    use crate::cancel::CancelToken;
    use crate::tools::ToolSource;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::agent::tool_roster::RosterStream;
    use crate::tools::test_support::stub_ctx;
    use caudra_providers::InvalidToolInput;

    const READ: &str = "read";
    const PARK: &str = "park";
    const BODY: &str = "file contents";
    const BATCH_ID: &str = "toolu_01";
    const NEVER_RAN: &str = "the adopted run must be the one already started";
    const SECOND_ID: &str = "toolu_02";
    const THIRD_ID: &str = "toolu_03";
    const WIRE_READ: &str = "wire_read";
    const REPEAT_THRESHOLD: usize = 3;
    const FOURTH_ID: &str = "toolu_04";
    const EXPECT_BATCH: &str = "expected adopted batch entries";
    const WIRE_BATCH: &str = "mcp_Batch";
    const MCP_BATCH: &str = "server__batch";
    const MCP_BATCH_INTERNAL: &str = "server.batch";
    const TEST_PATH: &str = "a.rs";
    const CHILD_COUNT: usize = 3;

    #[test_case("unknown_batch", None, false; "unknown_fuzzy_parent")]
    #[test_case(MCP_BATCH, None, false; "registered_mcp_parent")]
    #[test_case(WIRE_BATCH, None, false; "unregistered_alias")]
    #[test_case(WIRE_BATCH, Some(MCP_BATCH), false; "alias_to_mcp_parent")]
    #[test_case("batch", Some(MCP_BATCH), false; "builtin_spelling_aliases_to_mcp")]
    #[test_case("batch", None, true; "builtin_parent")]
    #[test_case("functions.batch", None, true; "functions_prefix")]
    #[test_case(WIRE_BATCH, Some("batch"), true; "request_alias")]
    #[test_case("functions.mcp_Batch", Some("batch"), true; "prefixed_request_alias")]
    fn only_exact_resolved_batch_parents_admit_children(
        parent: &str,
        target: Option<&str>,
        admitted: bool,
    ) {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let mcp = crate::mcp::stub_session(&[(MCP_BATCH_INTERNAL, BODY)]);
            assert!(mcp.has_tool(MCP_BATCH_INTERNAL));
            let runs = SpeculativeRuns::new(&ctx, Some(mcp));
            runs.set_aliases(target.map(|target| {
                Arc::new(HashMap::from([(
                    crate::agent::streaming::canonical_tool_name(parent).into(),
                    target.into(),
                )]))
            }));
            runs.register(BATCH_ID, parent);
            let mut roster = RosterStream::new(parent, true).unwrap();
            let input = format!(r#"{{"tool_calls":[{}]}}"#, element(TEST_PATH));
            for (index, entry) in roster.absorb(&input).ready {
                runs.start(BATCH_ID, index, &entry);
            }
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), usize::from(admitted));
            assert_eq!(runs.has_admitted(), admitted);
        });
    }

    #[test]
    fn children_without_registered_parent_are_not_admitted() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.start(BATCH_ID, 0, &element(TEST_PATH));
            runs.settled().await;
            assert!(!runs.has_admitted());
            assert_eq!(ran.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn local_batch_override_does_not_admit_builtin_children() {
        smol::block_on(async {
            let (mut ctx, ran) = counting_ctx();
            let local = ctx.local_tools[READ].clone();
            Arc::make_mut(&mut ctx.local_tools).insert(crate::tools::BATCH_TOOL_NAME.into(), local);
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element(TEST_PATH));
            runs.settled().await;
            assert!(!runs.has_admitted());
            assert_eq!(ran.load(Ordering::SeqCst), 0);
            let input = json!({"tool_calls": [{"tool": READ, "parameters": params(TEST_PATH)}]});
            runs.ready(BATCH_ID, crate::tools::BATCH_TOOL_NAME, input.clone());
            let done = runs
                .claim(BATCH_ID, crate::tools::BATCH_TOOL_NAME, &input)
                .unwrap()
                .finish()
                .await;
            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), BODY);
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    #[test_case(json!([{"tool": READ, "parameters": {"path": TEST_PATH}}]); "nested_array")]
    #[test_case(json!(r#"{"tool":"read","parameters":{"path":"a.rs"}}"#); "json_string")]
    #[test_case(Value::Null; "null")]
    #[test_case(json!(true); "boolean")]
    #[test_case(json!(42); "number")]
    fn invalid_direct_elements_never_execute_inner_or_later_children(invalid: Value) {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            let child: Value = serde_json::from_str(&element(TEST_PATH)).unwrap();
            let input = json!({"tool_calls": [invalid, child]});
            let mut roster = RosterStream::new(crate::tools::BATCH_TOOL_NAME, true).unwrap();
            for c in input.to_string().trim_end_matches('}').chars() {
                for (index, entry) in roster.absorb(&c.to_string()).ready {
                    runs.start(BATCH_ID, index, &entry);
                }
            }
            runs.settled().await;
            assert!(!runs.has_admitted());
            assert_eq!(ran.load(Ordering::SeqCst), 0);
            runs.ready(BATCH_ID, crate::tools::BATCH_TOOL_NAME, input.clone());
            let done = runs
                .claim(BATCH_ID, crate::tools::BATCH_TOOL_NAME, &input)
                .unwrap()
                .finish()
                .await;
            assert!(done.is_error);
            assert_eq!(ran.load(Ordering::SeqCst), 0);
        });
    }

    #[test_case(false, false, 2; "valid_finalized")]
    #[test_case(false, true, 2; "valid_adopted")]
    #[test_case(true, false, 2; "repaired_fallback")]
    #[test_case(true, true, 2; "repaired_mixed_adoption")]
    #[test_case(false, true, 1; "adopted_observations_reserved_once")]
    #[test_case(true, true, 1; "repaired_observations_reserved_once")]
    #[test_case(true, false, 0; "repaired_guard_disabled")]
    fn repaired_and_valid_batches_share_child_repeat_history(
        malformed: bool,
        streamed: bool,
        threshold: usize,
    ) {
        smol::block_on(async {
            let (mut ctx, ran) = counting_ctx();
            ctx.config.tool_json_repair = true;
            Arc::make_mut(&mut ctx.local_tools)
                .get_mut(READ)
                .unwrap()
                .effect = ToolEffect::Mutating;
            let observations = ResponseObservations::new(CHILD_COUNT * 2);
            ctx.steering_observations = Some(observations.clone());
            let runs = SpeculativeRuns::new(&ctx, None)
                .with_recent(RecentCalls::with_threshold(threshold));
            let child: Value = serde_json::from_str(&element(TEST_PATH)).unwrap();
            let input = json!({"tool_calls": vec![child; CHILD_COUNT]});
            for (parent_index, parent) in [BATCH_ID, SECOND_ID].into_iter().enumerate() {
                runs.register(parent, crate::tools::BATCH_TOOL_NAME);
                if streamed {
                    runs.start(parent, 0, &element(TEST_PATH));
                }
                let effective = if malformed { json!({}) } else { input.clone() };
                let mut response = StreamResponse {
                    message: Message {
                        content: vec![ContentBlock::tool_use(
                            parent,
                            crate::tools::BATCH_TOOL_NAME,
                            effective.clone(),
                        )],
                        ..Message::default()
                    },
                    ..StreamResponse::default()
                };
                if malformed {
                    response.invalid_tool_inputs.insert(
                        parent.into(),
                        InvalidToolInput {
                            raw: input
                                .to_string()
                                .replacen("\"tool_calls\"", "tool_calls", 1),
                            complete: true,
                            clipped: false,
                        },
                    );
                }
                runs.finalize(&mut response);
                let done = runs
                    .claim(parent, crate::tools::BATCH_TOOL_NAME, &effective)
                    .unwrap()
                    .finish()
                    .await;
                assert!(!done.is_error, "{done:?}");
                let ToolOutput::Batch { entries, .. } = done.output else {
                    panic!("{EXPECT_BATCH}")
                };
                assert_eq!(entries.len(), CHILD_COUNT);
                for (index, entry) in entries.iter().enumerate() {
                    let refused =
                        threshold > 0 && parent_index * CHILD_COUNT + index >= threshold - 1;
                    assert_eq!(
                        entry.status,
                        if refused {
                            BatchToolStatus::Error
                        } else {
                            BatchToolStatus::Success
                        }
                    );
                    if refused {
                        assert!(
                            entry
                                .output
                                .as_ref()
                                .unwrap()
                                .as_text()
                                .contains(crate::tools::DOOM_LOOP_GUIDANCE)
                        );
                    }
                }
            }
            assert_eq!(
                ran.load(Ordering::SeqCst),
                if threshold == 0 {
                    CHILD_COUNT * 2
                } else {
                    threshold - 1
                }
            );
            assert_eq!(
                runs.recent().is_doom_loop(READ, &params(TEST_PATH)),
                threshold > 0
            );
            let (facts, all_repairable) = observations.take();
            assert_eq!(facts.len(), CHILD_COUNT * 2);
            assert!(facts.iter().all(|fact| fact.name == READ));
            assert_eq!(all_repairable, threshold == 1);
            assert!(runs.drain_report().is_none());
        });
    }

    #[test_case(false ; "top_level")]
    #[test_case(true ; "batch_child")]
    fn cancellation_before_poll_prevents_effects_and_publishes_terminal_progress(child: bool) {
        smol::block_on(async {
            let (mut ctx, ran) = counting_ctx();
            let (events, received) = flume::unbounded();
            ctx.event_tx = crate::EventSender::new(events, 0);
            let (cancel, token) = CancelToken::new();
            ctx.cancel = token;
            cancel.cancel();
            let runs = SpeculativeRuns::new(&ctx, None);
            let id = if child {
                runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
                runs.start(BATCH_ID, 0, &element("a.rs"));
                batch::child_tool_use_id(Some(BATCH_ID), 0)
            } else {
                runs.ready(BATCH_ID, READ, params("a.rs"));
                BATCH_ID.into()
            };
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 0);
            if child {
                let peeked = runs.peek_all(BATCH_ID, [(READ, &params("a.rs"))]);
                let entry = peeked[0].as_ref().unwrap().entry().unwrap();
                assert_eq!(entry.status, BatchToolStatus::Error);
                assert_eq!(entry.tool, READ);
            }
            let done = runs
                .claim(&id, READ, &params("a.rs"))
                .unwrap()
                .finish()
                .await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), CANCELLED_EFFECTS);
            let events: Vec<_> = received.drain().map(|event| event.event).collect();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| match event {
                        AgentEvent::BatchProgress(progress) =>
                            child
                                && progress.id == BATCH_ID
                                && progress.entry.status == BatchToolStatus::Error,
                        AgentEvent::ToolDone(done) => !child && done.id == BATCH_ID,
                        _ => false,
                    })
                    .count(),
                1
            );
        });
    }

    #[test]
    fn final_reconciliation_preserves_original_observation_slots() {
        smol::block_on(async {
            let (mut ctx, ran) = counting_ctx();
            let observations = ResponseObservations::new(2);
            ctx.steering_observations = Some(observations.clone());
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, READ);
            runs.register(SECOND_ID, READ);
            runs.ready(SECOND_ID, READ, params("a.rs"));
            runs.settled().await;
            let mut response = StreamResponse {
                message: Message {
                    content: vec![ContentBlock::tool_use(THIRD_ID, READ, params("b.rs"))],
                    ..Message::default()
                },
                ..StreamResponse::default()
            };
            runs.reconcile(&mut response.message);
            runs.finalize(&mut response);
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 2);
            assert_eq!(response.message.tool_uses().count(), 2);
            assert!(
                !response
                    .message
                    .tool_uses()
                    .any(|(id, _, _)| id == BATCH_ID)
            );
            let (facts, _) = observations.take();
            assert_eq!(facts.len(), 2);
            assert_ne!(facts[0].fingerprint, facts[1].fingerprint);
        });
    }

    #[test]
    fn final_invalid_revision_cannot_replace_admitted_arguments() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.ready(BATCH_ID, READ, params("a.rs"));
            let mut response = StreamResponse {
                invalid_tool_inputs: HashMap::from([(
                    BATCH_ID.into(),
                    caudra_providers::InvalidToolInput {
                        raw: "{".into(),
                        complete: true,
                        clipped: false,
                    },
                )]),
                ..StreamResponse::default()
            };
            runs.finalize(&mut response);
            assert!(ctx.json_repair.invalid_input(BATCH_ID).is_none());
            assert!(response.invalid_tool_inputs.is_empty());
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 1);
            assert!(
                !runs
                    .claim(BATCH_ID, READ, &params("a.rs"))
                    .unwrap()
                    .finish()
                    .await
                    .is_error
            );
        });
    }

    #[test_case(false ; "final_only_aliases")]
    #[test_case(true ; "stream_resolved_alias")]
    fn final_aliases_adopt_the_same_tool_without_revision(streamed: bool) {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            if streamed {
                runs.ready(BATCH_ID, READ, params("a.rs"));
            }
            let mut response = StreamResponse {
                message: Message {
                    content: vec![ContentBlock::tool_use(BATCH_ID, WIRE_READ, params("a.rs"))],
                    ..Message::default()
                },
                tool_name_aliases: Some(Arc::new(HashMap::from([(WIRE_READ.into(), READ.into())]))),
                ..StreamResponse::default()
            };
            runs.finalize(&mut response);
            let done = runs
                .claim(BATCH_ID, WIRE_READ, &params("a.rs"))
                .unwrap()
                .finish()
                .await;
            assert!(!done.is_error);
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    #[test_case(false ; "top_level")]
    #[test_case(true ; "unfinished_batch")]
    fn final_text_preserves_begun_calls(child: bool) {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            if child {
                runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
                runs.start(BATCH_ID, 0, &element("a.rs"));
            } else {
                runs.ready(BATCH_ID, READ, params("a.rs"));
            }
            runs.settled().await;
            let mut response = StreamResponse {
                message: Message {
                    content: vec![ContentBlock::Text { text: BODY.into() }],
                    ..Message::default()
                },
                ..StreamResponse::default()
            };
            runs.reconcile(&mut response.message);
            runs.finalize(&mut response);
            let calls: Vec<_> = response.message.tool_uses().collect();
            assert_eq!(calls.len(), 1);
            let (id, name, input) = calls[0];
            assert_eq!(id, BATCH_ID);
            assert!(!runs.claim(id, name, input).unwrap().finish().await.is_error);
            assert_eq!(ran.load(Ordering::SeqCst), 1);
            assert!(runs.drain_report().is_none());
        });
    }

    #[test]
    fn revised_batch_parent_reports_children_without_running_a_different_tool() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            runs.ready(BATCH_ID, READ, params("b.rs"));
            let done = runs
                .claim(BATCH_ID, READ, &params("b.rs"))
                .unwrap()
                .finish()
                .await;
            assert!(done.is_error);
            let output = done.output.as_text();
            assert!(output.contains(REVISED_INPUT));
            assert!(output.contains("a.rs"));
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn cancellation_settles_an_admitted_noncooperative_local_tool() {
        smol::block_on(async {
            let (mut ctx, _) = counting_ctx();
            let (cancel, token) = CancelToken::new();
            ctx.cancel = token;
            let (started, running) = flume::unbounded();
            ctx.local_tools = Arc::new(HashMap::from([(
                PARK.into(),
                crate::tools::local_tool(move |_, _| {
                    let started = started.clone();
                    Box::pin(async move {
                        started.send(()).unwrap();
                        pending().await
                    })
                }),
            )]));
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.ready(BATCH_ID, PARK, Value::Null);
            running.recv_async().await.unwrap();
            cancel.cancel();
            runs.settled().await;
            let done = runs
                .claim(BATCH_ID, PARK, &Value::Null)
                .unwrap()
                .finish()
                .await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), CANCELLED_EFFECTS);
        });
    }

    #[test]
    fn outer_batch_adopts_children_without_owning_a_cycle() {
        smol::block_on(async {
            let (mut ctx, ran) = counting_ctx();
            let observations = ResponseObservations::new(1);
            ctx.steering_observations = Some(observations.clone());
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            let weak = Arc::downgrade(&runs);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            let input = json!({"tool_calls":[{"tool":READ,"parameters":params("a.rs")}]});
            runs.ready(BATCH_ID, crate::tools::BATCH_TOOL_NAME, input.clone());
            runs.settled().await;
            assert!(
                !runs
                    .claim(BATCH_ID, crate::tools::BATCH_TOOL_NAME, &input)
                    .unwrap()
                    .finish()
                    .await
                    .is_error
            );
            assert_eq!(ran.load(Ordering::SeqCst), 1);
            let (facts, repairable) = observations.take();
            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].name, READ);
            assert!(!repairable);
            drop(runs);
            assert!(weak.upgrade().is_none());
        });
    }

    #[test_case(false ; "ordinary")]
    #[test_case(true ; "alias")]
    fn top_level_slots_admit_once_and_adopt_exact_arguments(alias: bool) {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            let name = if alias { "wire_read" } else { READ };
            runs.set_aliases(Some(Arc::new(HashMap::from([(name.into(), READ.into())]))));
            runs.ready(BATCH_ID, name, params("a.rs"));
            runs.settled().await;
            let mut response = StreamResponse {
                message: Message {
                    content: vec![ContentBlock::tool_use(BATCH_ID, name, params("b.rs"))],
                    ..Message::default()
                },
                ..StreamResponse::default()
            };
            runs.finalize(&mut response);
            assert_eq!(ran.load(Ordering::SeqCst), 1);
            let done = runs
                .claim(BATCH_ID, name, &params("b.rs"))
                .unwrap()
                .finish()
                .await;
            assert!(done.output.as_text().contains(REVISED_INPUT));
            runs.finalize(&mut response);
            assert!(runs.claim(BATCH_ID, name, &params("b.rs")).is_none());
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    #[test_case(0, 2 ; "identical_distinct_slots")]
    #[test_case(2, 1 ; "repeat_threshold")]
    fn later_ready_slot_executes_before_earlier_input_and_checks_repeats(
        threshold: usize,
        effects: usize,
    ) {
        smol::block_on(async {
            let (mut ctx, ran) = counting_ctx();
            let observations = ResponseObservations::new(2);
            ctx.steering_observations = Some(observations.clone());
            let runs = SpeculativeRuns::new(&ctx, None)
                .with_recent(RecentCalls::with_threshold(threshold));
            runs.register(BATCH_ID, READ);
            runs.register(SECOND_ID, READ);
            runs.ready(SECOND_ID, READ, params("a.rs"));
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 1);
            runs.ready(SECOND_ID, READ, params("a.rs"));
            runs.ready(BATCH_ID, READ, params("a.rs"));
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), effects);
            assert_eq!(
                runs.claim(BATCH_ID, READ, &params("a.rs"))
                    .unwrap()
                    .finish()
                    .await
                    .is_error,
                threshold > 0
            );
            assert!(
                !runs
                    .claim(SECOND_ID, READ, &params("a.rs"))
                    .unwrap()
                    .finish()
                    .await
                    .is_error
            );
            let (facts, repairable) = observations.take();
            assert_eq!(facts.len(), 2);
            assert_eq!(
                facts[0].outcome,
                if threshold > 0 {
                    tool_dispatch::ToolOutcome::Repairable
                } else {
                    tool_dispatch::ToolOutcome::Success
                }
            );
            assert_eq!(facts[1].outcome, tool_dispatch::ToolOutcome::Success);
            assert!(!repairable);
        });
    }

    #[test_case(REPEAT_THRESHOLD, true ; "interleaved_unfinished_batches")]
    #[test_case(REPEAT_THRESHOLD, false ; "finalized_batches")]
    #[test_case(0, true ; "guard_disabled_distinct_children")]
    fn batch_child_admission_precedes_effects_and_survives_wrapper_adoption(
        threshold: usize,
        streamed: bool,
    ) {
        smol::block_on(async {
            let (mut ctx, ran) = counting_ctx();
            Arc::make_mut(&mut ctx.local_tools)
                .get_mut(READ)
                .unwrap()
                .effect = ToolEffect::Mutating;
            let runs = SpeculativeRuns::new(&ctx, None)
                .with_recent(RecentCalls::with_threshold(threshold));
            let parents = [BATCH_ID, SECOND_ID, THIRD_ID];
            let input = json!({"tool_calls":[
                {"tool":READ,"parameters":params("a.rs")},
                {"tool":READ,"parameters":params("a.rs")},
            ]});
            if streamed {
                for parent in parents {
                    runs.register(parent, crate::tools::BATCH_TOOL_NAME);
                    runs.start(parent, 0, &element("a.rs"));
                    runs.start(parent, 0, &element("a.rs"));
                }
                runs.settled().await;
                assert_eq!(
                    ran.load(Ordering::SeqCst),
                    if threshold == 0 {
                        parents.len()
                    } else {
                        threshold - 1
                    }
                );
            }
            for (parent_index, parent) in parents.into_iter().enumerate() {
                runs.ready(parent, crate::tools::BATCH_TOOL_NAME, input.clone());
                let done = runs
                    .claim(parent, crate::tools::BATCH_TOOL_NAME, &input)
                    .unwrap()
                    .finish()
                    .await;
                assert!(!done.is_error);
                let ToolOutput::Batch { entries, .. } = done.output else {
                    panic!("{EXPECT_BATCH}");
                };
                assert_eq!(entries.len(), 2);
                for (index, entry) in entries.iter().enumerate() {
                    let admitted_index = if streamed {
                        if index == 0 {
                            parent_index
                        } else {
                            parents.len() + parent_index
                        }
                    } else {
                        parent_index * entries.len() + index
                    };
                    let refused = threshold > 0 && admitted_index >= threshold - 1;
                    assert_eq!(
                        entry.status,
                        if refused {
                            BatchToolStatus::Error
                        } else {
                            BatchToolStatus::Success
                        }
                    );
                    if refused {
                        assert!(
                            entry
                                .output
                                .as_ref()
                                .unwrap()
                                .as_text()
                                .contains(crate::tools::DOOM_LOOP_GUIDANCE)
                        );
                    } else {
                        assert_eq!(entry.output.as_ref().unwrap().as_text(), BODY);
                    }
                }
                runs.ready(parent, crate::tools::BATCH_TOOL_NAME, input.clone());
                assert!(
                    runs.claim(parent, crate::tools::BATCH_TOOL_NAME, &input)
                        .is_none()
                );
            }
            runs.ready(FOURTH_ID, crate::tools::BATCH_TOOL_NAME, input.clone());
            let done = runs
                .claim(FOURTH_ID, crate::tools::BATCH_TOOL_NAME, &input)
                .unwrap()
                .finish()
                .await;
            let ToolOutput::Batch { entries, .. } = done.output else {
                panic!("{EXPECT_BATCH}");
            };
            assert!(entries.iter().all(|entry| entry.status
                == if threshold == 0 {
                    BatchToolStatus::Success
                } else {
                    BatchToolStatus::Error
                }));
            assert_eq!(
                ran.load(Ordering::SeqCst),
                if threshold == 0 {
                    (parents.len() + 1) * entries.len()
                } else {
                    threshold - 1
                }
            );
            assert!(runs.drain_report().is_none());
        });
    }

    #[test]
    fn rejected_wrapper_retains_child_results_and_repeat_history() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None).with_recent(RecentCalls::with_threshold(2));
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            let input = json!({"tool_calls":false});
            runs.ready(BATCH_ID, crate::tools::BATCH_TOOL_NAME, input.clone());
            let done = runs
                .claim(BATCH_ID, crate::tools::BATCH_TOOL_NAME, &input)
                .unwrap()
                .finish()
                .await;
            assert!(done.is_error);
            assert!(done.output.as_text().contains(BODY));
            assert!(done.output.as_text().contains("a.rs"));
            runs.ready(BATCH_ID, crate::tools::BATCH_TOOL_NAME, input.clone());
            assert!(
                runs.claim(BATCH_ID, crate::tools::BATCH_TOOL_NAME, &input)
                    .is_none()
            );
            runs.ready(SECOND_ID, READ, params("a.rs"));
            let repeated = runs
                .claim(SECOND_ID, READ, &params("a.rs"))
                .unwrap()
                .finish()
                .await;
            assert!(repeated.is_error);
            assert!(
                repeated
                    .output
                    .as_text()
                    .contains(crate::tools::DOOM_LOOP_GUIDANCE)
            );
            assert_eq!(ran.load(Ordering::SeqCst), 1);
            assert!(runs.drain_report().is_none());
        });
    }

    #[test]
    fn duplicate_child_completion_does_not_launch_again() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    /// A context with a tool that counts the times it ran, so "started once"
    /// is something a test can assert rather than infer, and one that never
    /// answers, so "still going" is not a race.
    fn counting_ctx() -> (ToolContext, Arc<AtomicUsize>) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry
            .register(
                Arc::new(batch::BatchTool),
                ToolSource::Native {
                    owner: crate::tools::native::OWNER.into(),
                    contract: crate::tools::BATCH_TOOL_NAME.into(),
                    trusted: true,
                },
            )
            .unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&ran);
        ctx.local_tools = Arc::new(HashMap::from([
            (
                READ.into(),
                crate::tools::local_tool(move |_, _| {
                    let count = Arc::clone(&count);
                    Box::pin(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(BODY.into())
                    })
                }),
            ),
            (
                PARK.into(),
                crate::tools::local_tool(|_, _| Box::pin(pending())),
            ),
        ]));
        (ctx, ran)
    }

    fn element(path: &str) -> String {
        call(READ, path)
    }

    fn call(tool: &str, path: &str) -> String {
        json!({ "tool": tool, "parameters": { "path": path } }).to_string()
    }

    fn params(path: &str) -> Value {
        json!({ "path": path })
    }

    #[test]
    fn a_claim_matches_the_stable_slot() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 3, &element("a.rs"));
            runs.settled().await;
            let adopted = runs
                .claim(
                    &batch::child_tool_use_id(Some(BATCH_ID), 3),
                    READ,
                    &params("a.rs"),
                )
                .expect(NEVER_RAN);
            assert_eq!(adopted.finish().await.output.as_text(), BODY);
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_call_the_stream_never_started_is_not_claimable() {
        smol::block_on(async {
            let (ctx, _ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            assert!(runs.claim("other", READ, &params("a.rs")).is_none());
            let adopted = runs
                .claim(
                    &batch::child_tool_use_id(Some(BATCH_ID), 0),
                    READ,
                    &params("b.rs"),
                )
                .unwrap();
            let done = adopted.finish().await;
            assert!(done.is_error);
            assert!(done.output.as_text().contains(REVISED_INPUT));
        });
    }

    /// Two children asking for the same thing are two calls, and the batch
    /// owes the model an answer for each.
    #[test]
    fn identical_children_claim_one_run_each() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.start(BATCH_ID, 1, &element("a.rs"));
            runs.settled().await;
            assert!(
                runs.claim(
                    &batch::child_tool_use_id(Some(BATCH_ID), 0),
                    READ,
                    &params("a.rs")
                )
                .is_some()
            );
            assert!(
                runs.claim(
                    &batch::child_tool_use_id(Some(BATCH_ID), 1),
                    READ,
                    &params("a.rs")
                )
                .is_some()
            );
            assert!(
                runs.claim(
                    &batch::child_tool_use_id(Some(BATCH_ID), 1),
                    READ,
                    &params("a.rs")
                )
                .is_none()
            );
            assert_eq!(ran.load(Ordering::SeqCst), 2);
        });
    }

    #[test_case(true ; "a_nested_batch")]
    #[test_case(false ; "a_name_this_context_cannot_resolve")]
    fn an_element_the_batch_must_answer_for_itself_is_never_started(nested: bool) {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            let tool = if nested {
                crate::tools::BATCH_TOOL_NAME
            } else {
                "mcp_Read"
            };
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(
                BATCH_ID,
                0,
                &json!({ "tool": tool, "parameters": {} }).to_string(),
            );
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 0);
            assert_eq!(runs.drain_report().is_none(), nested);
        });
    }

    /// The point of the report: work whose effects are already in place, that
    /// nothing in the answer accounts for.
    #[test]
    fn what_ran_and_was_never_claimed_is_reported_once() {
        smol::block_on(async {
            let (ctx, _ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.start(BATCH_ID, 1, &element("b.rs"));
            runs.settled().await;
            runs.claim(
                &batch::child_tool_use_id(Some(BATCH_ID), 0),
                READ,
                &params("a.rs"),
            )
            .expect(NEVER_RAN);
            let report = runs.drain_report().expect("an unclaimed run is reported");
            let text = report.first_text_content().unwrap_or_default().to_owned();
            assert!(text.contains(NOTICE_INTRO), "{text}");
            assert!(text.contains("b.rs"), "the unclaimed call is named: {text}");
            assert!(!text.contains("a.rs"), "the claimed one is not: {text}");
            assert!(
                runs.drain_report().is_none(),
                "a report is made once, not once per request"
            );
        });
    }

    /// A retry throws the message away, so a child still running loses the row
    /// it reports to. What already answered is kept: the retried message can
    /// still claim it, and the report still names it if nothing does.
    #[test]
    fn a_discarded_attempt_keeps_its_answers_and_drops_the_rest() {
        smol::block_on(async {
            let (ctx, _ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            runs.start(BATCH_ID, 1, &call(PARK, "b.rs"));
            runs.abandon_unfinished();
            assert!(
                runs.claim(
                    &batch::child_tool_use_id(Some(BATCH_ID), 0),
                    READ,
                    &params("a.rs")
                )
                .is_some()
            );
            assert!(
                runs.claim(
                    &batch::child_tool_use_id(Some(BATCH_ID), 1),
                    PARK,
                    &params("b.rs")
                )
                .is_none()
            );
        });
    }
}
