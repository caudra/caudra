//! Home Assistant's MCP Server integration over streamable HTTP, on the demo devices. These
//! tests share one Home Assistant, so nextest runs them one at a time, and each sets the
//! devices it uses before it starts. An intent returns before a slow device finishes, and the
//! demo lock takes seconds, so its state is read once it settles. To Home Assistant, turning a
//! lock on locks it.

use std::path::PathBuf;

use caudra_agent::AgentMode;
use caudra_agent::code::output::CodeCallStatus;
use caudra_agent::permissions::{PermissionAnswer, PermissionMode};
use serde_json::{Value, json};

use crate::support::{
    HomeAssistant, PLAN_FILE, PLAN_REFUSAL, Servers, completed, home_assistant,
    whole_tool_for_the_session,
};

const BED_LIGHT: &str = "light.bed_light";
const CEILING_LIGHTS: &str = "light.ceiling_lights";
const FRONT_DOOR: &str = "lock.front_door";
const LIGHT: &str = "light";
const LOCK: &str = "lock";
const UNLOCK: &str = "unlock";
const TURN_ON: &str = "turn_on";
const TURN_OFF: &str = "turn_off";
const ON: &str = "on";
const LOCKED: &str = "locked";
const UNLOCKED: &str = "unlocked";
const TURN_ON_KEY: &str = "home-assistant.intent__HassTurnOn";
const LIVE_CONTEXT: &str = "homeassistant__GetLiveContext";
const TURN_OFF_TOOL: &str = "intent__HassTurnOff";
const ANSWERED: &str = "answered";
const RUNTIME_ERROR: &str = "RuntimeError";
const IMPORTED_ASYNCIO: &str = "imported `asyncio`";
const DROPPED_RETURN: &str = "dropped `return` from the last line";
// MONTY-GATHER-SHIM: gone once Monty's `gather` takes `return_exceptions`.
const SETTLED: &str = "ran `asyncio.gather` without `return_exceptions`";

fn connect(mode: AgentMode, permission_mode: PermissionMode) -> Servers {
    Servers::connect(vec![home_assistant()], mode, permission_mode)
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn an_intent_changes_the_device() {
    let home = HomeAssistant::running();
    home.call(LIGHT, TURN_OFF, BED_LIGHT);
    let servers = connect(AgentMode::Build, PermissionMode::Yolo);
    let output = servers.run("await home_assistant.intent__HassTurnOn(name='Bed Light')");
    completed(&output);
    assert_eq!(output.calls[0].status, CodeCallStatus::Success);
    assert_eq!(home.settled(BED_LIGHT, ON), ON);
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn plan_mode_reads_live_context_and_refuses_an_intent() {
    let home = HomeAssistant::running();
    home.call(LIGHT, TURN_ON, CEILING_LIGHTS);
    let servers = connect(
        AgentMode::Plan(PathBuf::from(PLAN_FILE)),
        PermissionMode::Yolo,
    );
    let output = servers.run(
        "context = await home_assistant.homeassistant__GetLiveContext()\n\
         try:\n    await home_assistant.intent__HassTurnOff(name='Ceiling Lights')\n    refused = ''\n\
         except PermissionError as error:\n    refused = str(error)\n\
         refused",
    );
    assert!(completed(&output).contains(PLAN_REFUSAL), "{output:?}");
    let statuses: Vec<_> = output
        .calls
        .iter()
        .map(|call| (call.tool.as_str(), call.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (LIVE_CONTEXT, CodeCallStatus::Success),
            (TURN_OFF_TOOL, CodeCallStatus::Refused)
        ]
    );
    assert_eq!(home.state(CEILING_LIGHTS), ON);
}

/// A session allow covers later calls of the tool, except one naming a lock, which asks every
/// time.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_session_allow_never_covers_a_lock() {
    let home = HomeAssistant::running();
    home.call(LIGHT, TURN_OFF, CEILING_LIGHTS);
    home.call(LIGHT, TURN_OFF, BED_LIGHT);
    home.set(LOCK, UNLOCK, FRONT_DOOR, UNLOCKED);
    let servers = connect(AgentMode::Build, PermissionMode::Ask);
    let (output, asked) = servers.run_answering(
        "await home_assistant.intent__HassTurnOn(name='Ceiling Lights')\n\
         await home_assistant.intent__HassTurnOn(name='Bed Light')\n\
         await home_assistant.intent__HassTurnOn(name='Front Door', domain=['lock'])",
        &[whole_tool_for_the_session(), PermissionAnswer::AllowOnce],
    );
    completed(&output);
    let keys: Vec<String> = asked
        .iter()
        .map(|request| request.tool.to_string())
        .collect();
    assert_eq!(keys, [TURN_ON_KEY, TURN_ON_KEY]);
    assert_eq!(
        [(CEILING_LIGHTS, ON), (BED_LIGHT, ON), (FRONT_DOOR, LOCKED)]
            .map(|(entity, expected)| home.settled(entity, expected)),
        [ON, ON, LOCKED]
    );
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_denied_lock_stays_as_it_was() {
    let home = HomeAssistant::running();
    home.set(LOCK, UNLOCK, FRONT_DOOR, UNLOCKED);
    let servers = connect(AgentMode::Build, PermissionMode::Ask);
    let (output, asked) = servers.run_answering(
        "await home_assistant.intent__HassTurnOn(name='Front Door', domain=['lock'])",
        &[PermissionAnswer::Deny],
    );
    assert_eq!(asked.len(), 1);
    assert_eq!(output.calls[0].status, CodeCallStatus::Refused);
    assert_eq!(home.state(FRONT_DOOR), UNLOCKED);
}

/// The shape a model wrote against Home Assistant: `asyncio` unimported, `return_exceptions`, one
/// call that fails, and a final `return`. It runs, and the failure comes back as a value. The
/// settled repair is MONTY-GATHER-SHIM.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_gather_that_returns_its_exceptions_runs_as_written() {
    HomeAssistant::running();
    let servers = connect(AgentMode::Build, PermissionMode::Yolo);
    let output = servers.run(
        "results = await asyncio.gather(\n\
         \x20   home_assistant.homeassistant__GetLiveContext(),\n\
         \x20   home_assistant.intent__HassTurnOn(name='No Such Device'),\n\
         \x20   return_exceptions=True,\n\
         )\n\
         return [type(r).__name__ if isinstance(r, Exception) else 'answered' for r in results]",
    );
    let settled: Value = serde_json::from_str(completed(&output)).expect("result JSON");
    assert_eq!(settled, json!([ANSWERED, RUNTIME_ERROR]));
    assert_eq!(output.repairs, [IMPORTED_ASYNCIO, DROPPED_RETURN, SETTLED]);
    assert_eq!(output.lessons.len(), 2, "{output:?}");
}
