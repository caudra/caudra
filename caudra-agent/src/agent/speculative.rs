//! `batch` children started while the model is still writing the call.
//!
//! A batch spends its whole stream listing other calls, and each of those is
//! complete long before the list is. Every element that closes is dispatched
//! here at once, under the id the batch would have given it anyway, and the
//! batch adopts the running call instead of starting a second one.
//!
//! A run is claimed by what it is, not by where it sat: the key is the tool
//! plus its canonical arguments. That is what survives a retry, where the
//! provider throws the half-written message away and the model writes it
//! again, and it is why an identical child pair still claims two runs.
//!
//! Nothing here decides whether a call is allowed. Children go through
//! [`tool_dispatch::run`] like every other call, so permissions, mode gates,
//! revert points and path locks all apply exactly as they would have.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};

use async_lock::OnceCell;
use futures_lite::FutureExt;
use serde_json::Value;

use super::tool_dispatch::{self, Emit};
use crate::cancel::CancelTrigger;
use crate::mcp::McpSession;
use crate::permissions::canonical_json;
use crate::tools::ToolContext;
use crate::tools::native::batch;
use crate::types::{BatchToolEntry, ToolDoneEvent, ToolStartEvent};
use caudra_providers::Message;

const PANICKED: &str = "internal error: tool panicked";
const NOTICE_INTRO: &str = "These calls started while your last message was still being written and have already run, so their effects are in place. Do not repeat them:";
const NOTICE_OK: &str = "ok";
const NOTICE_ERROR: &str = "error";
const NOTICE_ELLIPSIS: &str = "…";
const SUMMARY_CAP: usize = 120;

/// Every child this response started before the message carrying it was whole.
pub struct SpeculativeRuns {
    /// The turn's context, minus anything a child must not inherit.
    ctx: ToolContext,
    mcp: Option<McpSession>,
    runs: Mutex<Vec<Run>>,
}

struct Run {
    key: u64,
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
}

impl Adopted {
    /// The header the call introduced itself with, once it has.
    pub fn start(&self) -> Option<&ToolStartEvent> {
        self.start.as_ref()
    }

    pub async fn finish(self) -> ToolDoneEvent {
        self.result.wait().await.clone()
    }
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
        let mut entry = batch::started_entry(self.start.as_ref()?);
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
        ctx.live_sink = None;
        // Reservations are the response's to make, and this call is not part
        // of one yet. The batch that adopts the run reserves it in order.
        ctx.steering_observations = None;
        ctx.steering_order = Vec::new();
        Self {
            ctx,
            mcp,
            runs: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Run>> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts the call `element` describes, if it is one this context can run
    /// on its own. A name that only the response's alias map could resolve is
    /// left for the batch, which will have the map by then.
    pub fn start(&self, batch_id: &str, index: usize, element: &str) {
        let Ok(entry) = serde_json::from_str::<Value>(element) else {
            return;
        };
        let Some((tool, params)) = batch::dispatchable(&entry, &self.ctx) else {
            return;
        };
        if !tool_dispatch::resolves_natively(&self.ctx, &tool) {
            return;
        }
        let request = shortened(&canonical_json(&params));
        let key = fingerprint(&tool, &params);
        let (cancel, token) = self.ctx.cancel.child();
        let mut ctx = self.ctx.clone();
        ctx.tool_use_id = Some(batch::child_tool_use_id(Some(batch_id), index));
        ctx.root_tool_use_id = ctx
            .root_tool_use_id
            .clone()
            .or_else(|| Some(batch_id.to_owned()));
        ctx.cancel = token;
        let start = Arc::new(Mutex::new(None));
        let result = Arc::new(OnceCell::new());
        let task = smol::spawn(run_child(
            ctx,
            self.mcp.clone(),
            tool.clone(),
            params,
            index,
            Arc::clone(&start),
            Arc::clone(&result),
        ));
        tracing::debug!(tool = %tool, index, batch_id, "started a batch child early");
        self.lock().push(Run {
            key,
            tool,
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
        calls: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Vec<Option<Peeked>> {
        let runs = self.lock();
        let mut taken = vec![false; runs.len()];
        calls
            .into_iter()
            .map(|(tool, params)| {
                let at = position(&runs, &taken, tool, params)?;
                taken[at] = true;
                Some(Peeked {
                    start: runs[at].started(),
                    done: runs[at].result.get().cloned(),
                })
            })
            .collect()
    }

    /// Takes over the run matching this call, if there is one.
    pub fn claim(&self, tool: &str, params: &Value) -> Option<Adopted> {
        let mut runs = self.lock();
        let at = position(&runs, &[], tool, params)?;
        let run = runs.remove(at);
        Some(Adopted {
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
    #[cfg(test)]
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
            lines.push(match named.is_empty() {
                true => format!("- {} ({outcome})", run.tool),
                false => format!("- {}: {named} ({outcome})", run.tool),
            });
        }
        (!lines.is_empty())
            .then(|| Message::observation(format!("{NOTICE_INTRO}\n{}", lines.join("\n"))))
    }
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
fn position(runs: &[Run], taken: &[bool], tool: &str, params: &Value) -> Option<usize> {
    let key = fingerprint(tool, params);
    runs.iter().enumerate().position(|(at, run)| {
        !taken.get(at).copied().unwrap_or(false) && run.key == key && run.tool == tool
    })
}

fn fingerprint(tool: &str, params: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    tool.hash(&mut hasher);
    canonical_json(params).hash(&mut hasher);
    hasher.finish()
}

/// One child, from its own dispatch to the slot the batch reads it from. The
/// row is published as it moves, so a child that started early is watched
/// early too, on the roster the stream has already drawn.
async fn run_child(
    ctx: ToolContext,
    mcp: Option<McpSession>,
    tool: String,
    params: Value,
    index: usize,
    start: Arc<Mutex<Option<ToolStartEvent>>>,
    result: Arc<OnceCell<ToolDoneEvent>>,
) {
    let id = ctx.tool_use_id.clone().unwrap_or_default();
    let done = {
        let (ctx, start) = (&ctx, &start);
        let call = async {
            tool_dispatch::run(
                &ctx.registry,
                mcp.as_ref(),
                id.clone(),
                &tool,
                &params,
                ctx,
                Emit::Capture(&mut |event: &ToolStartEvent| {
                    *start
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event.clone());
                    batch::publish_child(ctx, index, batch::started_entry(event));
                }),
            )
            .await
        };
        std::panic::AssertUnwindSafe(call)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| ToolDoneEvent::error(id, PANICKED))
    };
    if let Some(mut entry) = start
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(batch::started_entry)
    {
        batch::settle_entry(&mut entry, &done);
        batch::publish_child(&ctx, index, entry);
    }
    let _ = result.set(done).await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    const READ: &str = "read";
    const PARK: &str = "park";
    const BODY: &str = "file contents";
    const BATCH_ID: &str = "toolu_01";
    const NEVER_RAN: &str = "the adopted run must be the one already started";

    /// A context with a tool that counts the times it ran, so "started once"
    /// is something a test can assert rather than infer, and one that never
    /// answers, so "still going" is not a race.
    fn counting_ctx() -> (ToolContext, Arc<AtomicUsize>) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        let ran = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&ran);
        let (_never, parked) = flume::unbounded::<()>();
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
                crate::tools::local_tool(move |_, _| {
                    let parked = parked.clone();
                    Box::pin(async move {
                        let _ = parked.recv_async().await;
                        Ok(BODY.into())
                    })
                }),
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

    /// An index moves when the model rewrites its message; what the call is
    /// does not, which is why the claim keys on that instead.
    #[test]
    fn a_claim_matches_the_call_wherever_it_ended_up() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.start(BATCH_ID, 3, &element("a.rs"));
            runs.settled().await;
            let adopted = runs.claim(READ, &params("a.rs")).expect(NEVER_RAN);
            assert_eq!(adopted.finish().await.output.as_text(), BODY);
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_call_the_stream_never_started_is_not_claimable() {
        smol::block_on(async {
            let (ctx, _ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            assert!(runs.claim(READ, &params("b.rs")).is_none());
            assert!(runs.claim("other", &params("a.rs")).is_none());
        });
    }

    /// Two children asking for the same thing are two calls, and the batch
    /// owes the model an answer for each.
    #[test]
    fn identical_children_claim_one_run_each() {
        smol::block_on(async {
            let (ctx, ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.start(BATCH_ID, 1, &element("a.rs"));
            runs.settled().await;
            assert!(runs.claim(READ, &params("a.rs")).is_some());
            assert!(runs.claim(READ, &params("a.rs")).is_some());
            assert!(runs.claim(READ, &params("a.rs")).is_none());
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
            runs.start(
                BATCH_ID,
                0,
                &json!({ "tool": tool, "parameters": {} }).to_string(),
            );
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 0);
            assert!(runs.drain_report().is_none());
        });
    }

    /// The point of the report: work whose effects are already in place, that
    /// nothing in the answer accounts for.
    #[test]
    fn what_ran_and_was_never_claimed_is_reported_once() {
        smol::block_on(async {
            let (ctx, _ran) = counting_ctx();
            let runs = SpeculativeRuns::new(&ctx, None);
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.start(BATCH_ID, 1, &element("b.rs"));
            runs.settled().await;
            runs.claim(READ, &params("a.rs")).expect(NEVER_RAN);
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
            runs.start(BATCH_ID, 0, &element("a.rs"));
            runs.settled().await;
            runs.start(BATCH_ID, 1, &call(PARK, "b.rs"));
            runs.abandon_unfinished();
            assert!(runs.claim(READ, &params("a.rs")).is_some());
            assert!(runs.claim(PARK, &params("b.rs")).is_none());
        });
    }
}
