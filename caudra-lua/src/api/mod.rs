pub(crate) mod agent;
pub(crate) mod r#async;
pub(crate) mod autocmd;
pub(crate) mod base64;
pub(crate) mod env;
pub(crate) mod r#fn;
pub(crate) mod fs;
pub(crate) mod image;
pub(crate) mod interpreter;
pub(crate) mod json;
pub(crate) mod keymap;
pub(crate) mod log;
pub(crate) mod model;
pub(crate) mod net;
pub(crate) mod options;
pub(crate) mod session;
pub(crate) mod slot;
pub(crate) mod split;
pub(crate) mod task;
pub(crate) mod text;
pub(crate) mod tool;
pub(crate) mod treesitter;
pub(crate) mod ui;
pub(crate) mod util;
pub(crate) mod uv;
pub(crate) mod yaml;

use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::options::PluginOpts;
use crate::api::tool::{PendingRules, PendingTools, PermissionRulePolicy};
use crate::api::util::command::UiAction;
use crate::plugin_permissions::PluginPermissions;

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_caudra_global(
    lua: &Lua,
    pending: PendingTools,
    pending_rules: PendingRules,
    rule_policy: PermissionRulePolicy,
    plugin: Arc<str>,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    permissions: &PluginPermissions,
    opts: PluginOpts,
) -> LuaResult<Table> {
    let caudra = lua.create_table()?;

    let api = tool::create_api_table(
        lua,
        pending,
        pending_rules,
        rule_policy,
        Arc::clone(&plugin),
        opts,
        ui_action_tx.clone(),
    )?;
    autocmd::add_autocmd_methods(&api, lua, Arc::clone(&plugin))?;
    slot::add_slot_methods(&api, lua, Arc::clone(&plugin))?;
    caudra.set("api", api)?;
    caudra.set("env", env::create_env_table(lua, permissions)?)?;
    caudra.set("fs", fs::create_fs_table(lua, permissions)?)?;
    caudra.set("log", log::create_log_table(lua, Arc::clone(&plugin))?)?;
    caudra.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    caudra.set("uv", uv::create_uv_table(lua, permissions)?)?;
    caudra.set("base64", base64::create_base64_table(lua)?)?;
    caudra.set("image", image::create_image_table(lua)?)?;
    caudra.set("json", json::create_json_table(lua)?)?;
    caudra.set("yaml", yaml::create_yaml_table(lua)?)?;
    caudra.set("net", net::create_net_table(lua, permissions)?)?;
    caudra.set("text", text::create_text_table(lua)?)?;
    caudra.set(
        "session",
        session::create_session_table(lua, ui_action_tx.clone())?,
    )?;
    caudra.set(
        "model",
        model::create_model_table(lua, ui_action_tx.clone())?,
    )?;
    caudra.set("task", task::create_task_table(lua, ui_action_tx.clone())?)?;
    caudra.set(
        "ui",
        ui::create_ui_table(lua, ui_action_tx.clone(), Arc::clone(&plugin))?,
    )?;
    caudra.set(
        "fn",
        r#fn::create_fn_table(lua, Arc::clone(&plugin), permissions, ui_action_tx)?,
    )?;
    split::split__register(&caudra, lua)?;
    caudra.set("async", r#async::create_async_table(lua)?)?;
    caudra.set(
        "interpreter",
        interpreter::create_interpreter_table(lua, permissions)?,
    )?;
    caudra.set("agent", agent::create_agent_table(lua)?)?;
    caudra.set(
        "keymap",
        keymap::create_keymap_table(lua, Arc::clone(&plugin))?,
    )?;

    Ok(caudra)
}
