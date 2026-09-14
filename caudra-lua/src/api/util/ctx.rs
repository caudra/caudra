use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use caudra_agent::agent::LoadedInstructions;
use caudra_agent::cancel::CancelToken;
use caudra_agent::tools::{
    Deadline, FileReadTracker, LocalTools, ToolAudience, ToolContext, ToolLive,
};
use caudra_config::{AgentConfig, ToolOutputLines};
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::tool_outputs::{
    TOOL_OUTPUT_CONTROL_RESERVE_BYTES, ToolOutputId, ToolOutputRef, ToolOutputSink, ToolOutputStore,
};
use mlua::{LuaSerdeExt, MultiValue, UserData, UserDataMethods, Value as LuaValue};

use crate::api::tool::ToolCallReply;
use crate::api::ui::buf::BufHandle;
use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, err_pair};
use crate::runtime::{active_task, lock_cell};

const DEADLINE_ALREADY_SET_MSG: &str = "ctx:set_deadline() already called";
const TOOL_OUTPUT_SESSION_REQUIRED_MSG: &str = "tool output retrieval requires a session";
const TOOL_OUTPUT_STORE_UNAVAILABLE_MSG: &str = "tool output store is unavailable";
const TOOL_OUTPUT_SINK_CLOSED_MSG: &str = "tool output sink is already finished or discarded";
const TOOL_OUTPUT_SINK_WORKER_FAILED_MSG: &str = "tool output sink writer stopped unexpectedly";
const TOOL_OUTPUT_SINK_CHANNEL_CAPACITY: usize = 8;

pub(crate) struct ManagedToolOutputRef(ToolOutputRef);

impl ManagedToolOutputRef {
    pub(crate) fn reference(&self) -> &ToolOutputRef {
        &self.0
    }
}

impl UserData for ManagedToolOutputRef {}

enum ToolOutputSinkCommand {
    Append {
        text: String,
        reserve_bytes: usize,
        reply: flume::Sender<Result<(), String>>,
    },
    Finish(flume::Sender<Result<ToolOutputRef, String>>),
    Discard(flume::Sender<Result<(), String>>),
}

struct ToolOutputSinkWriter {
    tx: Option<flume::Sender<ToolOutputSinkCommand>>,
    thread: Option<JoinHandle<()>>,
}

impl ToolOutputSinkWriter {
    fn spawn(sink: ToolOutputSink) -> Result<Self, String> {
        let (tx, rx) = flume::bounded(TOOL_OUTPUT_SINK_CHANNEL_CAPACITY);
        let thread = thread::Builder::new()
            .name("tool-output-writer".into())
            .spawn(move || {
                let mut sink = Some(sink);
                while let Ok(command) = rx.recv() {
                    match command {
                        ToolOutputSinkCommand::Append {
                            text,
                            reserve_bytes,
                            reply,
                        } => {
                            let result = sink
                                .as_mut()
                                .ok_or_else(|| TOOL_OUTPUT_SINK_CLOSED_MSG.to_owned())
                                .and_then(|sink| {
                                    sink.append_with_reserve(&text, reserve_bytes)
                                        .map_err(|error| error.to_string())
                                });
                            let _ = reply.send(result);
                        }
                        ToolOutputSinkCommand::Finish(reply) => {
                            let result = sink
                                .take()
                                .ok_or_else(|| TOOL_OUTPUT_SINK_CLOSED_MSG.to_owned())
                                .and_then(|sink| sink.finish().map_err(|error| error.to_string()));
                            let _ = reply.send(result);
                            return;
                        }
                        ToolOutputSinkCommand::Discard(reply) => {
                            let result = sink
                                .take()
                                .ok_or_else(|| TOOL_OUTPUT_SINK_CLOSED_MSG.to_owned())
                                .and_then(|sink| sink.discard().map_err(|error| error.to_string()));
                            let _ = reply.send(result);
                            return;
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    fn append(&self, text: String, reserve_bytes: usize) -> Result<(), String> {
        let Some(tx) = self.tx.as_ref() else {
            return Err(TOOL_OUTPUT_SINK_CLOSED_MSG.to_owned());
        };
        let (reply, result) = flume::bounded(1);
        tx.send(ToolOutputSinkCommand::Append {
            text,
            reserve_bytes,
            reply,
        })
        .map_err(|_| TOOL_OUTPUT_SINK_WORKER_FAILED_MSG.to_owned())?;
        result
            .recv()
            .map_err(|_| TOOL_OUTPUT_SINK_WORKER_FAILED_MSG.to_owned())?
    }

    fn finish(mut self) -> Result<ToolOutputRef, String> {
        let Some(tx) = self.tx.as_ref() else {
            return Err(TOOL_OUTPUT_SINK_CLOSED_MSG.to_owned());
        };
        let (reply, result) = flume::bounded(1);
        tx.send(ToolOutputSinkCommand::Finish(reply))
            .map_err(|_| TOOL_OUTPUT_SINK_WORKER_FAILED_MSG.to_owned())?;
        let result = result
            .recv()
            .map_err(|_| TOOL_OUTPUT_SINK_WORKER_FAILED_MSG.to_owned())?;
        self.close()?;
        result
    }

    fn discard(mut self) -> Result<(), String> {
        let Some(tx) = self.tx.as_ref() else {
            return Err(TOOL_OUTPUT_SINK_CLOSED_MSG.to_owned());
        };
        let (reply, result) = flume::bounded(1);
        tx.send(ToolOutputSinkCommand::Discard(reply))
            .map_err(|_| TOOL_OUTPUT_SINK_WORKER_FAILED_MSG.to_owned())?;
        let result = result
            .recv()
            .map_err(|_| TOOL_OUTPUT_SINK_WORKER_FAILED_MSG.to_owned())?;
        self.close()?;
        result
    }

    fn close(&mut self) -> Result<(), String> {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| TOOL_OUTPUT_SINK_WORKER_FAILED_MSG.to_owned())?;
        }
        Ok(())
    }
}

impl Drop for ToolOutputSinkWriter {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct LuaToolOutputSink(Option<ToolOutputSinkWriter>);

impl UserData for LuaToolOutputSink {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("append", |_, this, text: String| {
            let Some(sink) = this.0.as_ref() else {
                return Ok(err_pair(TOOL_OUTPUT_SINK_CLOSED_MSG));
            };
            match sink.append(text, 0) {
                Ok(()) => Ok((Some(true), None)),
                Err(error) => Ok(err_pair(error)),
            }
        });

        methods.add_method_mut("append_process_output", |_, this, text: String| {
            let Some(sink) = this.0.as_ref() else {
                return Ok(err_pair(TOOL_OUTPUT_SINK_CLOSED_MSG));
            };
            match sink.append(text, TOOL_OUTPUT_CONTROL_RESERVE_BYTES) {
                Ok(()) => Ok((Some(true), None)),
                Err(error) => Ok(err_pair(error)),
            }
        });

        methods.add_method_mut("append_control", |_, this, text: String| {
            let Some(sink) = this.0.as_ref() else {
                return Ok(err_pair(TOOL_OUTPUT_SINK_CLOSED_MSG));
            };
            match sink.append(text, 0) {
                Ok(()) => Ok((Some(true), None)),
                Err(error) => Ok(err_pair(error)),
            }
        });

        methods.add_method_mut("finish", |lua, this, ()| {
            let Some(sink) = this.0.take() else {
                return Ok(err_pair(TOOL_OUTPUT_SINK_CLOSED_MSG));
            };
            match sink.finish() {
                Ok(reference) => Ok((
                    Some(lua.create_userdata(ManagedToolOutputRef(reference))?),
                    None,
                )),
                Err(error) => Ok(err_pair(error)),
            }
        });

        methods.add_method_mut("discard", |_, this, ()| {
            let Some(sink) = this.0.take() else {
                return Ok(err_pair(TOOL_OUTPUT_SINK_CLOSED_MSG));
            };
            match sink.discard() {
                Ok(()) => Ok((Some(true), None)),
                Err(error) => Ok(err_pair(error)),
            }
        });
    }
}

fn send_live_buf(lua: &mlua::Lua, buf: &mlua::AnyUserData) -> mlua::Result<()> {
    let shared = buf.borrow::<BufHandle>().map(|h| Arc::clone(&h.buf))?;
    let task = active_task(lua);
    let (live, sink) = {
        let mut cell = lock_cell(&task);
        cell.root_buf = Some(Arc::clone(&shared));
        (cell.live.clone(), cell.live_sink.clone())
    };
    if let Some(live) = live {
        let _ = live.event_tx.send(caudra_agent::AgentEvent::LiveToolBuf {
            id: live.tool_use_id.clone(),
            body: Arc::clone(&shared),
        });
    }
    if let Some(sink) = sink {
        let _ = sink.send(ToolLive::Buf(shared));
    }
    Ok(())
}

/// Captured snapshot of the parent `ToolContext`. Per-call state (deadline,
/// instructions, output lines) is reset so child calls start clean.
#[derive(Clone)]
pub(crate) struct AgentContext {
    tool: ToolContext,
    caller: Option<AgentCaller>,
}

#[derive(Clone)]
struct AgentCaller {
    tool: Arc<str>,
    bundled: bool,
}

impl From<&ToolContext> for AgentContext {
    fn from(ctx: &ToolContext) -> Self {
        let mut c = ctx.clone();
        c.loaded_instructions = LoadedInstructions::new();
        c.deadline = Deadline::None;
        c.tool_output_lines = ToolOutputLines::default();
        c.local_tools = LocalTools::default();
        // Nested Lua dispatch is an implementation detail of the outer tool,
        // not another model-selected attempt. It must neither repair the
        // parent's failure nor replace its observation by expanding a batch.
        c.steering_observations = None;
        c.steering_order.clear();
        Self {
            tool: c,
            caller: None,
        }
    }
}

impl Deref for AgentContext {
    type Target = ToolContext;
    fn deref(&self) -> &ToolContext {
        &self.tool
    }
}

impl AgentContext {
    /// Drops `tool_use_id` so an inner tool never emits UI events under the
    /// outer call's id, and `live_sink` so a grandchild never streams into
    /// a sink meant for its parent.
    pub(crate) fn to_tool_context(&self) -> ToolContext {
        let mut c = self.tool.clone();
        c.tool_use_id = None;
        c.live_sink = None;
        c
    }

    pub(crate) fn caller_is_bundled_tool(&self, name: &str) -> bool {
        self.caller
            .as_ref()
            .is_some_and(|caller| caller.bundled && caller.tool.as_ref() == name)
    }

    fn with_caller(mut self, tool: Arc<str>, bundled: bool) -> Self {
        self.caller = Some(AgentCaller { tool, bundled });
        self
    }
}

/// One ctx type for handler, `start`, and restore invocations. Each kind's
/// capabilities live in its `Caps` variant, so a capability exists exactly
/// when its data does. Methods a kind lacks return `(nil, err)` instead of
/// not existing, so callers can probe without pcall.
pub(crate) struct LuaCtx {
    caps: Caps,
    pub(crate) cancel: CancelToken,
    tool_output_lines: ToolOutputLines,
    pub(crate) finish_tx: Option<flume::Sender<ToolCallReply>>,
}

enum Caps {
    Handler {
        agent: Box<AgentContext>,
        /// Kept apart from `agent`, which resets its copy so child calls
        /// start with a clean instruction set.
        loaded_instructions: LoadedInstructions,
    },
    /// `start` runs after permission checks: it reads config and publishes
    /// previews, but dispatching tools is structurally impossible.
    Start {
        config: AgentConfig,
        audience: ToolAudience,
        session_id: Option<SessionRef>,
        read_only: bool,
    },
    Restore {
        state: Option<serde_json::Value>,
    },
}

impl LuaCtx {
    fn new(ctx: &ToolContext, caps: Caps) -> Self {
        Self {
            caps,
            cancel: ctx.cancel.clone(),
            tool_output_lines: ctx.tool_output_lines,
            finish_tx: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn handler(ctx: &ToolContext) -> Self {
        Self::new(
            ctx,
            Caps::Handler {
                agent: Box::new(AgentContext::from(ctx)),
                loaded_instructions: ctx.loaded_instructions.clone(),
            },
        )
    }

    pub(crate) fn handler_for_tool(ctx: &ToolContext, tool: Arc<str>, bundled: bool) -> Self {
        Self::new(
            ctx,
            Caps::Handler {
                agent: Box::new(AgentContext::from(ctx).with_caller(tool, bundled)),
                loaded_instructions: ctx.loaded_instructions.clone(),
            },
        )
    }

    pub(crate) fn start(ctx: &ToolContext) -> Self {
        Self::new(
            ctx,
            Caps::Start {
                config: ctx.config.clone(),
                audience: ctx.audience,
                session_id: ctx.session_id.clone(),
                read_only: !matches!(ctx.mode, caudra_agent::AgentMode::Build)
                    || ctx.policy().is_read_only(),
            },
        )
    }

    pub(crate) fn restore(
        tool_output_lines: ToolOutputLines,
        state: Option<serde_json::Value>,
    ) -> Self {
        Self {
            caps: Caps::Restore { state },
            cancel: CancelToken::none(),
            tool_output_lines,
            finish_tx: None,
        }
    }

    /// Dispatch capability: only handler ctxs can call `caudra.agent.*`.
    pub(crate) fn agent(&self) -> Option<&AgentContext> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(agent),
            _ => None,
        }
    }

    pub(crate) fn is_read_only(&self) -> bool {
        match &self.caps {
            Caps::Handler { agent, .. } => {
                !matches!(agent.mode, caudra_agent::AgentMode::Build)
                    || agent.policy().is_read_only()
            }
            Caps::Start { read_only, .. } => *read_only,
            Caps::Restore { .. } => false,
        }
    }

    fn config(&self) -> Option<&AgentConfig> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(&agent.config),
            Caps::Start { config, .. } => Some(config),
            Caps::Restore { .. } => None,
        }
    }

    fn audience(&self) -> Option<ToolAudience> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(agent.audience),
            Caps::Start { audience, .. } => Some(*audience),
            Caps::Restore { .. } => None,
        }
    }

    /// Outer `None` means the kind has no session at all, inner `None`
    /// means this run has one but it is not tied to a session.
    fn session_id(&self) -> Option<Option<&SessionRef>> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(agent.session_id.as_ref()),
            Caps::Start { session_id, .. } => Some(session_id.as_ref()),
            Caps::Restore { .. } => None,
        }
    }

    fn file_tracker(&self) -> Option<&FileReadTracker> {
        self.agent().map(|a| &*a.file_tracker)
    }

    fn loaded_instructions(&self) -> Option<&LoadedInstructions> {
        match &self.caps {
            Caps::Handler {
                loaded_instructions,
                ..
            } => Some(loaded_instructions),
            _ => None,
        }
    }

    fn state(&self) -> Option<&serde_json::Value> {
        match &self.caps {
            Caps::Restore { state } => state.as_ref(),
            _ => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self.caps {
            Caps::Handler { .. } => "handler",
            Caps::Start { .. } => "start",
            Caps::Restore { .. } => "restore",
        }
    }

    pub(crate) fn cap_err(&self, method: &str) -> String {
        format!("{method} not available in {} ctx", self.kind())
    }

    fn cap_err_pair<T>(&self, method: &str) -> Pair<T> {
        (None, Some(self.cap_err(method)))
    }

    fn tool_output_access(&self, method: &str) -> Result<(CaudraId, Arc<ToolOutputStore>), String> {
        let Some(agent) = self.agent() else {
            return Err(self.cap_err(method));
        };
        let Some(session_id) = &agent.session_id else {
            return Err(TOOL_OUTPUT_SESSION_REQUIRED_MSG.into());
        };
        let Some(store) = &agent.tool_output_store else {
            return Err(TOOL_OUTPUT_STORE_UNAVAILABLE_MSG.into());
        };
        Ok((session_id.id(), Arc::clone(store)))
    }
}

fn positive_tool_output_arg(value: i64, name: &str) -> Result<usize, String> {
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{name} must be at least 1"))
}

fn tool_output_context_arg(value: i64, name: &str) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("{name} must be non-negative"))
}

impl UserData for LuaCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancelled", |_, this, ()| Ok(this.cancel.is_cancelled()));

        methods.add_method("audience", |_, this, ()| {
            let Some(audience) = this.audience() else {
                return Ok(this.cap_err_pair("audience"));
            };
            Ok((Some(audience.name().unwrap_or("main").to_string()), None))
        });

        methods.add_method("canonical_tool_name", |_, this, name: String| {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("canonical_tool_name"));
            };
            Ok((Some(agent.resolve_tool_name_alias(&name).to_owned()), None))
        });

        // The session that called this tool, which under concurrent
        // sessions is not always the focused one `caudra.session.current()`
        // reports. Nil without an error when the run has no session, as in
        // the `caudra index` one-shot.
        methods.add_method("session_id", |_, this, ()| {
            let Some(session_id) = this.session_id() else {
                return Ok(this.cap_err_pair("session_id"));
            };
            let Some(session_id) = session_id else {
                return Ok((None, None));
            };
            Ok((Some(session_id.id().to_string()), None))
        });

        methods.add_method("live_buf", |lua, this, buf: mlua::AnyUserData| {
            if matches!(this.caps, Caps::Restore { .. }) {
                return Ok(this.cap_err_pair("live_buf"));
            }
            send_live_buf(lua, &buf)?;
            Ok((Some(true), None))
        });

        methods.add_method("config", |lua, this, args: MultiValue| {
            let Some(config) = this.config() else {
                return Ok(this.cap_err_pair("config"));
            };
            let config_val = lua.to_value(config)?;
            if args.is_empty() {
                return Ok((Some(config_val), None));
            }
            let key: String = lua.from_value(args[0].clone())?;
            let default = args.get(1).cloned().unwrap_or(LuaValue::Nil);
            let val = match config_val {
                LuaValue::Table(ref tbl) => {
                    let val = tbl.raw_get::<LuaValue>(key.as_str())?;
                    if matches!(val, LuaValue::Nil) {
                        default
                    } else {
                        val
                    }
                }
                _ => default,
            };
            Ok((Some(val), None))
        });

        methods.add_method("tool_output_lines", |lua, this, ()| {
            lua.to_value(&this.tool_output_lines)
        });

        methods.add_method("tool_output_sink", |lua, this, ()| {
            let (session_id, store) = match this.tool_output_access("tool_output_sink") {
                Ok(access) => access,
                Err(error) => return Ok(err_pair(error)),
            };
            match store.begin(session_id) {
                Ok(sink) => match ToolOutputSinkWriter::spawn(sink) {
                    Ok(sink) => Ok((
                        Some(lua.create_userdata(LuaToolOutputSink(Some(sink)))?),
                        None,
                    )),
                    Err(error) => Ok(err_pair(error)),
                },
                Err(error) => Ok(err_pair(error)),
            }
        });

        methods.add_async_method(
            "tool_output_read",
            |lua,
             this,
             (raw_id, offset, limit, byte_offset): (String, i64, i64, Option<i64>)| async move {
                let (session_id, store) = match this.tool_output_access("tool_output_read") {
                    Ok(access) => access,
                    Err(error) => return Ok(err_pair(error)),
                };
                let id = match raw_id.parse::<ToolOutputId>() {
                    Ok(id) => id,
                    Err(error) => return Ok(err_pair(format!("invalid tool output ID: {error}"))),
                };
                let offset = match positive_tool_output_arg(offset, "offset") {
                    Ok(offset) => offset,
                    Err(error) => return Ok(err_pair(error)),
                };
                let limit = match positive_tool_output_arg(limit, "limit") {
                    Ok(limit) => limit,
                    Err(error) => return Ok(err_pair(error)),
                };
                let byte_offset = match tool_output_context_arg(
                    byte_offset.unwrap_or_default(),
                    "byte_offset",
                ) {
                    Ok(byte_offset) => byte_offset,
                    Err(error) => return Ok(err_pair(error)),
                };
                drop(this);

                let result = smol::unblock(move || {
                    store.read_at(session_id, id, offset, limit, byte_offset)
                })
                .await;
                let result = match result {
                    Ok(result) => result,
                    Err(error) => return Ok(err_pair(error)),
                };
                let json = serde_json::to_value(result).map_err(mlua::Error::external)?;
                let LuaValue::Table(table) = json_to_lua(&lua, &json)? else {
                    return Err(mlua::Error::runtime(
                        "tool output read result did not serialize to a table",
                    ));
                };
                Ok((Some(table), None))
            },
        );

        methods.add_async_method(
            "tool_output_grep",
            |lua,
             this,
             (raw_id, pattern, offset, limit, context_before, context_after): (
                String,
                String,
                i64,
                i64,
                i64,
                i64,
            )| async move {
                let (session_id, store) = match this.tool_output_access("tool_output_grep") {
                    Ok(access) => access,
                    Err(error) => return Ok(err_pair(error)),
                };
                let id = match raw_id.parse::<ToolOutputId>() {
                    Ok(id) => id,
                    Err(error) => return Ok(err_pair(format!("invalid tool output ID: {error}"))),
                };
                let offset = match positive_tool_output_arg(offset, "offset") {
                    Ok(offset) => offset,
                    Err(error) => return Ok(err_pair(error)),
                };
                let limit = match positive_tool_output_arg(limit, "limit") {
                    Ok(limit) => limit,
                    Err(error) => return Ok(err_pair(error)),
                };
                let context_before = match tool_output_context_arg(context_before, "context_before")
                {
                    Ok(context) => context,
                    Err(error) => return Ok(err_pair(error)),
                };
                let context_after = match tool_output_context_arg(context_after, "context_after") {
                    Ok(context) => context,
                    Err(error) => return Ok(err_pair(error)),
                };
                drop(this);

                let result = smol::unblock(move || {
                    store.grep(
                        session_id,
                        id,
                        &pattern,
                        offset,
                        limit,
                        context_before,
                        context_after,
                    )
                })
                .await;
                let result = match result {
                    Ok(result) => result,
                    Err(error) => return Ok(err_pair(error)),
                };
                let json = serde_json::to_value(result).map_err(mlua::Error::external)?;
                let LuaValue::Table(table) = json_to_lua(&lua, &json)? else {
                    return Err(mlua::Error::runtime(
                        "tool output grep result did not serialize to a table",
                    ));
                };
                Ok((Some(table), None))
            },
        );

        methods.add_method("state", |lua, this, ()| match this.state() {
            Some(v) => json_to_lua(lua, v),
            None => Ok(LuaValue::Nil),
        });

        methods.add_method("set_deadline", |lua, this, secs: u64| {
            if !matches!(this.caps, Caps::Handler { .. }) {
                return Ok(this.cap_err_pair("set_deadline"));
            }
            let handle = active_task(lua);
            let cell = handle.lock().unwrap_or_else(|e| e.into_inner());
            if cell.deadline_secs.get().is_some() {
                return Err(mlua::Error::runtime(DEADLINE_ALREADY_SET_MSG));
            }
            cell.deadline_secs.set(Some(secs));
            cell.deadline
                .set(Some(Instant::now() + Duration::from_secs(secs)));
            cell.deadline_changed.notify(usize::MAX);
            Ok((Some(true), None))
        });

        methods.add_method("record_read", |_, this, path: String| {
            let Some(tracker) = this.file_tracker() else {
                return Ok(this.cap_err_pair("record_read"));
            };
            tracker.record_read(Path::new(&path));
            Ok((Some(true), None))
        });

        methods.add_method("check_before_edit", |_, this, path: String| {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("check_before_edit"));
            };
            if !agent.config.stale_read_check {
                return Ok((Some(true), None));
            }
            match agent.file_tracker.check_before_edit(Path::new(&path)) {
                Ok(()) => Ok((Some(true), None)),
                Err(msg) => Ok((Some(false), Some(msg))),
            }
        });

        methods.add_async_method(
            "find_instructions",
            |lua, this, dir_path: String| async move {
                let Some(loaded) = this.loaded_instructions().cloned() else {
                    return Ok(this.cap_err_pair("find_instructions"));
                };
                // Nothing may hold the ctx borrow across the wait: a cancel
                // hook firing meanwhile needs `ctx:finish`, which takes it
                // mutably.
                drop(this);
                let results = smol::unblock(move || {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let abs = resolve_abs_with_cwd(dir_path, &cwd);
                    caudra_agent::find_subdirectory_instructions(&abs, &cwd, &loaded)
                })
                .await;
                let tbl = lua.create_table()?;
                for (i, (path, content)) in results.into_iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("path", path)?;
                    entry.set("content", content)?;
                    tbl.set(i + 1, entry)?;
                }
                Ok((Some(tbl), None))
            },
        );

        methods.add_method("is_instruction_file", |_, _, name: String| {
            Ok(caudra_agent::is_instruction_file(&name))
        });

        methods.add_method_mut("finish", |lua, this, val: LuaValue| {
            if !matches!(this.caps, Caps::Handler { .. }) {
                return Ok(this.cap_err_pair("finish"));
            }
            let tx = this
                .finish_tx
                .take()
                .ok_or_else(|| mlua::Error::runtime("ctx:finish() already called"))?;

            if let Some(buf) = crate::api::ui::buf::buf_from_reply(&val) {
                lock_cell(&active_task(lua)).root_buf = Some(buf);
            }
            let _ = tx.send(ToolCallReply::from_lua_value(lua, &val));
            Ok((Some(true), None))
        });
    }
}

fn resolve_abs_with_cwd(path: String, cwd: &Path) -> PathBuf {
    if Path::new(&path).is_absolute() {
        path.into()
    } else {
        cwd.join(&path)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicBool, Ordering};

    use caudra_agent::AgentMode;
    use caudra_agent::agent::tool_dispatch::{self, Emit};
    use caudra_agent::tools::LocalToolFn;
    use caudra_agent::tools::native::batch::BatchTool;
    use caudra_agent::tools::registry::ToolSource;
    use caudra_agent::tools::test_support::{observe_tool_calls, stub_ctx_with};
    use futures::FutureExt;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const TOOL_USE_ID: &str = "tu-1";
    const INSTRUCTION_PATH: &str = "/tmp/nested/AGENTS.md";
    const LOCAL_TOOL_NAME: &str = "sess_tool";
    const OUTER_FAILURE: &str = "outer wrapper failed";
    const OBSERVATION_WINDOW: usize = 3;
    /// Arbitrary ids are rejected: `SessionRef` parses base58 or a uuid.
    const SESSION_ID: &str = "CNK1hV6GWoysH3KQMm5wu";

    fn session_ref() -> SessionRef {
        SESSION_ID.parse().expect("valid session id")
    }

    fn populated_ctx() -> ToolContext {
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(TOOL_USE_ID));
        ctx.session_id = Some(session_ref());
        ctx.deadline = Deadline::after(Duration::from_secs(60));
        ctx.tool_output_lines = ToolOutputLines {
            bash: 999,
            ..ToolOutputLines::default()
        };
        assert!(
            !ctx.loaded_instructions
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH))
        );
        let mut tools: HashMap<String, LocalToolFn> = HashMap::new();
        tools.insert(
            LOCAL_TOOL_NAME.into(),
            caudra_agent::tools::local_tool(|_, _| Box::pin(async { Ok(String::new()) })),
        );
        ctx.local_tools = Arc::new(tools);
        ctx.live_sink = Some(flume::unbounded().0);
        ctx.steering_order = vec![1, 2];
        ctx
    }

    #[test]
    fn agent_context_keeps_tool_use_id_and_resets_per_call_state() {
        let agent = AgentContext::from(&populated_ctx());
        assert_eq!(agent.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert_eq!(
            agent.session_id,
            Some(session_ref()),
            "the session owns the whole run, so it is not per-call state"
        );
        assert!(matches!(agent.deadline, Deadline::None));
        assert_eq!(agent.tool_output_lines, ToolOutputLines::default());
        assert!(agent.local_tools.is_empty());
        assert!(agent.steering_order.is_empty());
        assert!(
            !agent
                .loaded_instructions
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH)),
            "loaded_instructions must be a fresh set, not a shared clone"
        );
    }

    #[test]
    fn agent_context_to_tool_context_drops_tool_use_id_and_sink() {
        let agent = AgentContext::from(&populated_ctx());
        assert!(
            agent.live_sink.is_some(),
            "the sink set by the caller must survive into AgentContext"
        );
        let inner = agent.to_tool_context();
        assert_eq!(inner.tool_use_id, None);
        assert!(inner.live_sink.is_none(), "sink must not be inherited");
        assert_eq!(agent.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert_eq!(
            inner.session_id,
            Some(session_ref()),
            "a dispatched child runs in the same session, unlike tool_use_id"
        );
    }

    #[test_case(false, false; "invalid_child_then_failure")]
    #[test_case(false, true; "invalid_child_then_panic")]
    #[test_case(true, false; "nested_batch_then_outer_failure")]
    fn nested_dispatch_cannot_mutate_outer_observation(batch: bool, panics: bool) {
        smol::block_on(async {
            let mut ctx = populated_ctx();
            let observations = observe_tool_calls(&mut ctx, OBSERVATION_WINDOW);
            ctx.registry
                .register(
                    Arc::new(BatchTool),
                    ToolSource::Native {
                        owner: LOCAL_TOOL_NAME.into(),
                        contract: caudra_agent::tools::BATCH_TOOL_NAME.into(),
                        trusted: true,
                    },
                )
                .unwrap();
            let reached_outer_failure = Arc::new(AtomicBool::new(false));
            let reached = Arc::clone(&reached_outer_failure);
            ctx.local_tools = Arc::new(HashMap::from([(
                LOCAL_TOOL_NAME.into(),
                caudra_agent::tools::local_tool(move |_, parent| {
                    let reached = Arc::clone(&reached);
                    Box::pin(async move {
                        let nested = AgentContext::from(&parent).to_tool_context();
                        let input = if batch {
                            json!({"tool_calls": [{"tool": "unknown_child", "parameters": {}}]})
                        } else {
                            json!({"tool_calls": []})
                        };
                        let done = tool_dispatch::run(
                            &nested.registry,
                            None,
                            String::new(),
                            caudra_agent::tools::BATCH_TOOL_NAME,
                            &input,
                            &nested,
                            Emit::Silent,
                        )
                        .await;
                        assert_eq!(done.is_error, !batch);
                        reached.store(true, Ordering::SeqCst);
                        assert!(!panics, "{OUTER_FAILURE}");
                        Err(OUTER_FAILURE.into())
                    })
                }),
            )]));
            let input = json!({});
            let result = AssertUnwindSafe(tool_dispatch::run(
                &ctx.registry,
                None,
                TOOL_USE_ID.into(),
                LOCAL_TOOL_NAME,
                &input,
                &ctx,
                Emit::Silent,
            ))
            .catch_unwind()
            .await;
            assert!(reached_outer_failure.load(Ordering::SeqCst));
            if panics {
                assert!(result.is_err());
            } else {
                let done = result.unwrap();
                assert!(done.is_error);
                assert_eq!(done.output.as_text(), OUTER_FAILURE);
            }
            assert_eq!(observations(), (vec![LOCAL_TOOL_NAME.into()], false));
        });
    }

    #[test]
    fn bundled_caller_provenance_requires_matching_name_and_trust() {
        let ctx = populated_ctx();
        let trusted = AgentContext::from(&ctx).with_caller(Arc::from("task"), true);
        let untrusted = AgentContext::from(&ctx).with_caller(Arc::from("task"), false);

        assert!(trusted.caller_is_bundled_tool("task"));
        assert!(!trusted.caller_is_bundled_tool("batch"));
        assert!(!untrusted.caller_is_bundled_tool("task"));
    }

    #[test]
    fn session_id_reaches_handler_and_start_but_not_restore() {
        let ctx = populated_ctx();
        assert_eq!(
            LuaCtx::handler(&ctx).session_id(),
            Some(Some(&session_ref()))
        );
        assert_eq!(LuaCtx::start(&ctx).session_id(), Some(Some(&session_ref())));
        assert_eq!(
            LuaCtx::restore(ToolOutputLines::default(), None).session_id(),
            None,
            "restore has no ToolContext to take a session from"
        );
    }

    #[test]
    fn session_id_absent_is_distinct_from_kind_lacking_it() {
        let mut ctx = populated_ctx();
        ctx.session_id = None;
        assert_eq!(
            LuaCtx::handler(&ctx).session_id(),
            Some(None),
            "a sessionless run still has the capability, so lua sees nil without an error"
        );
    }

    #[test]
    fn handler_ctx_keeps_parent_instruction_set() {
        let ctx = LuaCtx::handler(&populated_ctx());
        assert!(
            ctx.loaded_instructions()
                .expect("handler has instructions")
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH)),
            "handler must share the parent's set; AgentContext resets its own copy"
        );
    }

    #[test]
    fn tool_output_access_is_handler_only() {
        let ctx = populated_ctx();
        assert_eq!(
            LuaCtx::start(&ctx)
                .tool_output_access("tool_output_sink")
                .unwrap_err(),
            "tool_output_sink not available in start ctx"
        );
        assert_eq!(
            LuaCtx::start(&ctx)
                .tool_output_access("tool_output_read")
                .unwrap_err(),
            "tool_output_read not available in start ctx"
        );
        assert_eq!(
            LuaCtx::restore(ToolOutputLines::default(), None)
                .tool_output_access("tool_output_grep")
                .unwrap_err(),
            "tool_output_grep not available in restore ctx"
        );
    }
}
