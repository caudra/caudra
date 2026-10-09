use caudra_config::{AgentConfig, effective_shell_execution};
use serde_json::Value;

use crate::prompt::{ResolvedSlots, execution_guidance, shell_execution_guidance};

use super::{DeferredTool, SHELL_TOOL_NAME, TASK_TOOL_NAME, native::task};

const SHELL_EXECUTION_HEADING: &str = "\n\nExecution:\n";

pub fn configure_declared(
    tools: &mut Value,
    config: &AgentConfig,
    tasks_supported: bool,
    shell_supported: bool,
) {
    task::configure_execution(tools, config, tasks_supported, shell_supported);
    let Some(mode) = effective_shell_execution(config, shell_supported) else {
        return;
    };
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
                &mode,
                config.shell_async_threshold_secs,
            ));
        }
    }
}

pub fn configure_tools(
    tools: &mut Value,
    deferred: &mut Vec<DeferredTool>,
    config: &AgentConfig,
    tasks_supported: bool,
    shell_supported: bool,
) {
    configure_declared(tools, config, tasks_supported, shell_supported);
    *deferred = std::mem::take(deferred)
        .into_iter()
        .filter_map(|tool| {
            let mut definitions = Value::Array(vec![tool.definition]);
            configure_declared(&mut definitions, config, tasks_supported, shell_supported);
            let definition = definitions.as_array_mut()?.pop()?;
            Some(DeferredTool::new(&tool.name, tool.group, definition))
        })
        .collect();
}

pub fn execution_slots(
    slots: &ResolvedSlots,
    config: &AgentConfig,
    tasks_supported: bool,
    shell_supported: bool,
    tools: &Value,
    deferred: &[DeferredTool],
) -> ResolvedSlots {
    let exposed = |name: &str| {
        tools.as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        }) || deferred.iter().any(|tool| tool.name.as_ref() == name)
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

    use caudra_config::{
        AgentConfig, ExecutionMode, effective_shell_execution, effective_task_execution,
    };
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{configure_declared, configure_tools, execution_slots};
    use crate::{
        agent::build_system_prompt,
        prompt::{self, PromptId, ResolvedSlots, Slot, SlotEntry},
        template::Vars,
        tools::{
            DescriptionContext, ToolAudience, ToolFilter,
            deferral::DeferralSession,
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
    const CUSTOM_THRESHOLD: u64 = 937;
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
        deferred: bool,
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
            if deferred { TOOL_NAMES } else { &[] },
        )
    }

    fn find<'a>(tools: &'a Value, name: &str) -> Option<&'a Value> {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
    }

    fn assert_contract(definition: Option<&Value>, mode: Option<ExecutionMode>, task: bool) {
        assert_eq!(definition.is_some(), mode.is_some());
        let (Some(definition), Some(mode)) = (definition, mode) else {
            return;
        };
        let text = definition.to_string().to_lowercase();
        match mode {
            ExecutionMode::Sync => {
                for forbidden in ["background", "async", "receipt", "promot"] {
                    assert!(!text.contains(forbidden), "{forbidden}: {text}");
                }
            }
            ExecutionMode::Auto => {
                assert!(text.contains("receipt"));
                assert!(text.contains(if task { "foreground" } else { "synchronously" }));
            }
            ExecutionMode::Async => {
                for forbidden in ["foreground", "synchronous", "background: false", "promot"] {
                    assert!(!text.contains(forbidden), "{forbidden}: {text}");
                }
                assert!(text.contains("receipt"));
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
            for background in [false, true] {
                let mut input = input.clone();
                input["background"] = json!(background);
                let valid = match mode {
                    ExecutionMode::Sync => false,
                    ExecutionMode::Auto => true,
                    ExecutionMode::Async => background,
                };
                assert_eq!(validator.is_valid(&input), valid);
            }
            let background = &schema["properties"]["background"];
            assert_eq!(
                background["default"],
                match mode {
                    ExecutionMode::Sync => Value::Null,
                    ExecutionMode::Auto => json!(false),
                    ExecutionMode::Async => json!(true),
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
            assert_eq!(*schema, ShellContract.schema());
            assert_eq!(
                text.contains(&CUSTOM_THRESHOLD.to_string()),
                mode == ExecutionMode::Auto
            );
            assert!(
                definition["description"]
                    .as_str()
                    .unwrap()
                    .contains(SHELL_DESCRIPTION)
            );
        }
    }

    #[test_case(ExecutionMode::Sync; "task_sync")]
    #[test_case(ExecutionMode::Auto; "task_auto")]
    #[test_case(ExecutionMode::Async; "task_async")]
    fn actual_inventory_and_prompt_match_execution_policy(task_mode: ExecutionMode) {
        let registry = registry();
        for shell_mode in [
            ExecutionMode::Sync,
            ExecutionMode::Auto,
            ExecutionMode::Async,
        ] {
            let config = AgentConfig {
                task_execution: task_mode.clone(),
                shell_execution: shell_mode,
                shell_async_threshold_secs: CUSTOM_THRESHOLD,
                ..AgentConfig::default()
            };
            for (task_cap, shell_cap) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                for (defer, examples) in
                    [(false, false), (false, true), (true, false), (true, true)]
                {
                    let mut definitions =
                        definitions(&registry, ToolAudience::MAIN, defer, examples);
                    configure_tools(
                        &mut definitions.declared,
                        &mut definitions.deferred,
                        &config,
                        task_cap,
                        shell_cap,
                    );
                    let slots = execution_slots(
                        &ResolvedSlots::default(),
                        &config,
                        task_cap,
                        shell_cap,
                        &definitions.declared,
                        &definitions.deferred,
                    );
                    let system = build_system_prompt(
                        "",
                        &slots,
                        &ToolFilter::Only(TOOL_NAMES.iter().map(|name| (*name).into()).collect()),
                        None,
                        None,
                    );
                    let effective_task = effective_task_execution(&config, task_cap);
                    let effective_shell = effective_shell_execution(&config, shell_cap);
                    if let Some(mode) = &effective_task {
                        assert!(system.contains(&prompt::task_execution_guidance(mode)));
                    } else {
                        assert!(!system.contains("Task calls"));
                    }
                    if let Some(mode) = &effective_shell {
                        assert!(
                            system.contains(&prompt::shell_execution_guidance(
                                mode,
                                CUSTOM_THRESHOLD
                            ))
                        );
                    } else {
                        assert!(!system.contains("Shell calls"));
                    }
                    let session = DeferralSession::new(definitions.deferred, std::iter::empty());
                    let mut catalog = definitions.declared.clone();
                    session.request_snapshot().extend_tools(&mut catalog);
                    for (name, available) in [
                        ("task", effective_task.is_some()),
                        ("shell", effective_shell.is_some()),
                        ("task_control", task_cap || shell_cap),
                    ] {
                        if defer {
                            let result = session.fresh().search(name).unwrap();
                            assert_eq!(
                                result.loaded.iter().any(|loaded| loaded.as_ref() == name),
                                available
                            );
                            session.mark_loaded(name);
                        }
                    }
                    let mut published = definitions.declared;
                    session.request_snapshot().extend_tools(&mut published);
                    assert_contract(find(&published, "task"), effective_task.clone(), true);
                    assert_contract(find(&published, "shell"), effective_shell.clone(), false);
                    if let Some(control) = find(&published, "task_control") {
                        assert_eq!(
                            control.to_string().contains("background"),
                            effective_task == Some(ExecutionMode::Auto)
                        );
                    }
                    if effective_task
                        .as_ref()
                        .is_none_or(|mode| *mode == ExecutionMode::Sync)
                        && effective_shell
                            .as_ref()
                            .is_none_or(|mode| *mode == ExecutionMode::Sync)
                    {
                        let all = format!("{system}\n{published}\n{catalog}").to_lowercase();
                        for forbidden in ["background", "async"] {
                            assert!(!all.contains(forbidden), "{forbidden}: {all}");
                            assert!(session.fresh().search(forbidden).unwrap().loaded.is_empty());
                        }
                    }
                    let once = published.clone();
                    configure_declared(&mut published, &config, task_cap, shell_cap);
                    assert_eq!(published, once);
                }
            }
        }
    }

    #[test_case(false; "prose_examples")]
    #[test_case(true; "structured_examples")]
    fn execution_reconfiguration_preserves_examples_and_canonical_facts(examples: bool) {
        let registry = registry();
        let mut definitions = definitions(&registry, ToolAudience::MAIN, true, examples);
        for mode in [
            ExecutionMode::Auto,
            ExecutionMode::Async,
            ExecutionMode::Sync,
            ExecutionMode::Auto,
        ] {
            let config = AgentConfig {
                task_execution: mode.clone(),
                shell_execution: mode.clone(),
                shell_async_threshold_secs: CUSTOM_THRESHOLD,
                ..AgentConfig::default()
            };
            configure_tools(
                &mut definitions.declared,
                &mut definitions.deferred,
                &config,
                true,
                true,
            );
            let session = DeferralSession::new(definitions.deferred.clone(), std::iter::empty());
            for name in TOOL_NAMES {
                session.mark_loaded(name);
            }
            let mut published = definitions.declared.clone();
            session.request_snapshot().extend_tools(&mut published);
            assert_contract(find(&published, "task"), Some(mode.clone()), true);
            assert_contract(find(&published, "shell"), Some(mode.clone()), false);
            let description = find(&published, "task").unwrap()["description"]
                .as_str()
                .unwrap();
            assert_eq!(
                description
                    .matches(&prompt::task_execution_guidance(&mode))
                    .count(),
                1
            );
            if mode == ExecutionMode::Sync {
                assert!(
                    session
                        .fresh()
                        .search("background")
                        .unwrap()
                        .loaded
                        .is_empty()
                );
            }
            if mode == ExecutionMode::Async {
                assert!(
                    session
                        .fresh()
                        .search("foreground")
                        .unwrap()
                        .loaded
                        .is_empty()
                );
            }
        }
    }

    #[test_case(ExecutionMode::Sync)]
    #[test_case(ExecutionMode::Auto)]
    #[test_case(ExecutionMode::Async)]
    fn child_inventory_replaces_inherited_execution_guidance(shell_mode: ExecutionMode) {
        let registry = registry();
        let config = AgentConfig {
            task_execution: ExecutionMode::Auto,
            shell_execution: shell_mode.clone(),
            ..AgentConfig::default()
        };
        let mut inherited = ResolvedSlots::default().with_execution_guidance(
            &prompt::execution_guidance(&AgentConfig::default(), true, true, true, true),
        );
        inherited.insert(
            PromptId::General,
            Slot::ToolUsage,
            SlotEntry {
                plugin: "custom".into(),
                content: CUSTOM_INSTRUCTION.into(),
            },
        );
        for supported in [false, true] {
            let mut definitions = definitions(&registry, ToolAudience::GENERAL_SUB, true, false);
            configure_tools(
                &mut definitions.declared,
                &mut definitions.deferred,
                &config,
                false,
                supported,
            );
            let slots = execution_slots(
                &inherited,
                &config,
                false,
                supported,
                &definitions.declared,
                &definitions.deferred,
            );
            let rendered = prompt::assemble_task(PromptId::General, &slots, "", None);
            assert!(rendered.contains(CUSTOM_INSTRUCTION));
            assert!(!rendered.contains("Task calls"));
            let session = DeferralSession::new(definitions.deferred, std::iter::empty());
            assert!(
                !session
                    .search("task")
                    .unwrap()
                    .loaded
                    .iter()
                    .any(|name| name.as_ref() == "task")
            );
            session.search("task_control").unwrap();
            let mut published = definitions.declared;
            session.request_snapshot().extend_tools(&mut published);
            assert_eq!(find(&published, "task_control").is_some(), supported);
            if let Some(control) = find(&published, "task_control") {
                assert!(!control.to_string().contains("background"));
                assert!(!control.to_string().contains("foreground"));
            }
            if let Some(mode) = effective_shell_execution(&config, supported) {
                assert!(rendered.contains(&prompt::shell_execution_guidance(
                    &mode,
                    config.shell_async_threshold_secs
                )));
            } else {
                assert!(!rendered.contains("Shell calls"));
            }
        }
    }
}
