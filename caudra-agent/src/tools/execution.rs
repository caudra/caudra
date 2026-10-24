use caudra_config::AgentConfig;
use serde_json::{Value, json};

use crate::prompt::{ResolvedSlots, execution_guidance, shell_execution_guidance};

use super::{
    SHELL_TOOL_NAME, TASK_TOOL_NAME,
    native::{question, task},
};

const SHELL_EXECUTION_HEADING: &str = "\n\nExecution:\n";
const SHELL_BACKGROUND_PROPERTY: &str = "background";
const SHELL_BACKGROUND_DESCRIPTION: &str = "Start the command in the background and return a receipt at once, for a server, a watcher, or anything expected to outlast the foreground limit. Its terminal result arrives automatically. Default false.";

pub fn configure_declared(
    tools: &mut Value,
    config: &AgentConfig,
    tasks_supported: bool,
    shell_supported: bool,
) {
    task::configure_execution(tools, config, tasks_supported, shell_supported);
    question::configure_deferral(tools, config.deferred_questions && tasks_supported);
    let Some(tools) = tools.as_array_mut() else {
        return;
    };
    for tool in tools {
        if tool.get("name").and_then(Value::as_str) != Some(SHELL_TOOL_NAME) {
            continue;
        }
        if let Some(Value::String(description)) = tool.get_mut("description") {
            if let Some(offset) = description.find(SHELL_EXECUTION_HEADING) {
                description.truncate(offset);
            }
            description.push_str(SHELL_EXECUTION_HEADING);
            description.push_str(&shell_execution_guidance(
                shell_supported,
                config.shell_foreground_limit_secs,
            ));
        }
        if let Some(properties) = tool
            .get_mut("input_schema")
            .and_then(|schema| schema.get_mut("properties"))
            .and_then(Value::as_object_mut)
        {
            match shell_supported {
                true => properties.insert(
                    SHELL_BACKGROUND_PROPERTY.into(),
                    json!({"type": "boolean", "default": false, "description": SHELL_BACKGROUND_DESCRIPTION}),
                ),
                false => properties.remove(SHELL_BACKGROUND_PROPERTY),
            };
        }
    }
}

pub fn execution_slots(
    slots: &ResolvedSlots,
    config: &AgentConfig,
    tasks_supported: bool,
    shell_supported: bool,
    tools: &Value,
) -> ResolvedSlots {
    let exposed = |name: &str| {
        tools.as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        })
    };
    slots.with_execution_guidance(&execution_guidance(
        config,
        tasks_supported,
        shell_supported,
        exposed(TASK_TOOL_NAME),
        exposed(SHELL_TOOL_NAME),
    ))
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, sync::Arc};

    use caudra_config::AgentConfig;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{configure_declared, execution_slots};
    use crate::{
        agent::build_system_prompt,
        prompt::{self, PromptId, ResolvedSlots, Slot, SlotEntry},
        template::Vars,
        tools::{
            DescriptionContext, ToolAudience, ToolFilter,
            native::{task::TaskTool, task_control::TaskControl},
            registry::{
                ParseError, Tool, ToolDefinitions, ToolInvocation, ToolRegistry, ToolSource,
            },
        },
    };

    const TOOL_NAMES: &[&str] = &["task", "shell", "task_control"];
    const SHELL_DESCRIPTION: &str = "Execute a Bash command. Workcell validates the requested timeout and enforces its deadline. Descendants retaining output pipes are terminated.";
    const SHELL_DEFAULT_TIMEOUT: u64 = 120;
    const SHELL_MAX_TIMEOUT: u64 = 21600;
    const CUSTOM_LIMIT_SECS: u64 = 937;
    const CUSTOM_LIMIT: &str = "15m37s";
    const PROFILE_SUMMARY: &str = "- builtin: Built-in delegation instructions";
    const CUSTOM_INSTRUCTION: &str =
        "Custom background and foreground wording must remain untouched.";
    const SHELL_NOT_EXECUTABLE: &str = "schema fixture has no executor";

    struct ShellContract;

    impl Tool for ShellContract {
        fn name(&self) -> &str {
            "shell"
        }
        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed(SHELL_DESCRIPTION)
        }
        fn schema(&self) -> Value {
            json!({"type":"object", "additionalProperties":false, "required":["command"], "properties":{
                "command":{"type":"string"},
                "timeoutSec":{"type":"integer", "minimum":1, "maximum":SHELL_MAX_TIMEOUT, "default":SHELL_DEFAULT_TIMEOUT},
                "workdir":{"type":"string"}
            }})
        }
        fn parse(&self, _: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Err(ParseError::custom(SHELL_NOT_EXECUTABLE))
        }
    }

    fn registry() -> ToolRegistry {
        let registry = ToolRegistry::new();
        for tool in [
            Arc::new(TaskTool) as Arc<dyn Tool>,
            Arc::new(ShellContract),
            Arc::new(TaskControl),
        ] {
            registry
                .register(
                    tool,
                    ToolSource::Native {
                        owner: "execution-tests".into(),
                        contract: "execution-tests/v1".into(),
                        trusted: true,
                    },
                )
                .unwrap();
        }
        registry
    }

    fn definitions(
        registry: &ToolRegistry,
        audience: ToolAudience,
        examples: bool,
    ) -> ToolDefinitions {
        registry.definitions_split(
            &Vars::new().set("{task_system_prompt_profiles}", PROFILE_SUMMARY),
            &DescriptionContext {
                filter: &ToolFilter::All,
                audience,
                workflows_available: false,
            },
            examples,
        )
    }

    fn find<'a>(tools: &'a Value, name: &str) -> Option<&'a Value> {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
    }

    fn assert_contract(definition: Option<&Value>, background: bool, task: bool) {
        let definition = definition.unwrap();
        let text = definition.to_string().to_lowercase();
        if background {
            assert!(text.contains("receipt"));
            assert!(text.contains(if task {
                "foreground"
            } else {
                "moves to the background"
            }));
        } else {
            for forbidden in ["background", "async", "receipt", "promot"] {
                assert!(!text.contains(forbidden), "{forbidden}: {text}");
            }
        }
        let schema = &definition["input_schema"];
        assert_eq!(schema["additionalProperties"], false);
        let validator = jsonschema::validator_for(schema).unwrap();
        if task {
            let input = json!({"description":"Inspect policy", "prompt":"Return findings"});
            assert!(validator.is_valid(&input));
            let mut unknown = input.clone();
            unknown["unknown"] = json!(true);
            assert!(!validator.is_valid(&unknown));
            for requested in [false, true] {
                let mut input = input.clone();
                input["background"] = json!(requested);
                assert_eq!(validator.is_valid(&input), background);
            }
            assert_eq!(
                schema["properties"]["background"]["default"],
                if background {
                    json!(false)
                } else {
                    Value::Null
                }
            );
            assert!(
                !schema["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("background"))
            );
            assert!(
                definition["description"]
                    .as_str()
                    .unwrap()
                    .contains(PROFILE_SUMMARY)
            );
            assert!(text.contains("find auth middleware"));
        } else {
            let mut expected = ShellContract.schema();
            if background {
                expected["properties"]["background"] = json!({"type": "boolean", "default": false, "description": super::SHELL_BACKGROUND_DESCRIPTION});
            }
            assert_eq!(*schema, expected);
            assert_eq!(text.contains(CUSTOM_LIMIT), background);
            let validator = jsonschema::validator_for(schema).unwrap();
            assert_eq!(
                validator.is_valid(&json!({"command": "make serve", "background": true})),
                background
            );
            assert!(
                definition["description"]
                    .as_str()
                    .unwrap()
                    .contains(SHELL_DESCRIPTION)
            );
        }
    }

    #[test]
    fn actual_inventory_and_prompt_match_session_support() {
        let registry = registry();
        let config = AgentConfig {
            shell_foreground_limit_secs: CUSTOM_LIMIT_SECS,
            ..AgentConfig::default()
        };
        for (task_cap, shell_cap) in [(false, false), (false, true), (true, false), (true, true)] {
            for examples in [false, true] {
                let mut definitions = definitions(&registry, ToolAudience::MAIN, examples);
                configure_declared(&mut definitions.declared, &config, task_cap, shell_cap);
                let slots = execution_slots(
                    &ResolvedSlots::default(),
                    &config,
                    task_cap,
                    shell_cap,
                    &definitions.declared,
                );
                let system = build_system_prompt(
                    "",
                    &slots,
                    &ToolFilter::Only(TOOL_NAMES.iter().map(|name| (*name).into()).collect()),
                    None,
                    None,
                );
                assert!(system.contains(&prompt::task_execution_guidance(task_cap)));
                assert!(system.contains(&prompt::shell_execution_guidance(
                    shell_cap,
                    CUSTOM_LIMIT_SECS
                )));
                let mut published = definitions.declared;
                assert_contract(find(&published, "task"), task_cap, true);
                assert_contract(find(&published, "shell"), shell_cap, false);
                assert_eq!(
                    find(&published, "task_control").is_some(),
                    task_cap || shell_cap
                );
                if let Some(control) = find(&published, "task_control") {
                    assert_eq!(control.to_string().contains("background"), task_cap);
                }
                if !task_cap && !shell_cap {
                    let all = format!("{system}\n{published}").to_lowercase();
                    for forbidden in ["background", "async"] {
                        assert!(!all.contains(forbidden), "{forbidden}: {all}");
                    }
                }
                let once = published.clone();
                configure_declared(&mut published, &config, task_cap, shell_cap);
                assert_eq!(published, once);
            }
        }
    }

    #[test_case(false; "prose_examples")]
    #[test_case(true; "structured_examples")]
    fn execution_reconfiguration_preserves_examples_and_canonical_facts(examples: bool) {
        let registry = registry();
        let mut definitions = definitions(&registry, ToolAudience::MAIN, examples);
        let config = AgentConfig {
            shell_foreground_limit_secs: CUSTOM_LIMIT_SECS,
            ..AgentConfig::default()
        };
        for (task_cap, shell_cap) in [(true, true), (false, false), (true, false), (false, true)] {
            configure_declared(&mut definitions.declared, &config, task_cap, shell_cap);
            let published = &definitions.declared;
            assert_contract(find(published, "task"), task_cap, true);
            assert_contract(find(published, "shell"), shell_cap, false);
            let description = find(published, "task").unwrap()["description"]
                .as_str()
                .unwrap();
            assert_eq!(
                description
                    .matches(&prompt::task_execution_guidance(task_cap))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn child_inventory_replaces_inherited_execution_guidance() {
        let registry = registry();
        let config = AgentConfig::default();
        let mut inherited = ResolvedSlots::default()
            .with_execution_guidance(&prompt::execution_guidance(&config, true, true, true, true));
        inherited.insert(
            PromptId::General,
            Slot::ToolUsage,
            SlotEntry {
                plugin: "custom".into(),
                content: CUSTOM_INSTRUCTION.into(),
            },
        );
        for supported in [false, true] {
            let mut definitions = definitions(&registry, ToolAudience::GENERAL_SUB, false);
            configure_declared(&mut definitions.declared, &config, false, supported);
            let slots =
                execution_slots(&inherited, &config, false, supported, &definitions.declared);
            let rendered = prompt::assemble_task(PromptId::General, &slots, "", None);
            assert!(rendered.contains(CUSTOM_INSTRUCTION));
            assert!(!rendered.contains("Task calls"));
            let published = definitions.declared;
            assert!(find(&published, "task").is_none());
            assert_eq!(find(&published, "task_control").is_some(), supported);
            if let Some(control) = find(&published, "task_control") {
                assert!(!control.to_string().contains("background"));
                assert!(!control.to_string().contains("foreground"));
            }
            assert!(rendered.contains(&prompt::shell_execution_guidance(
                supported,
                config.shell_foreground_limit_secs
            )));
        }
    }
}
