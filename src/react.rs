use crate::config::ToolConfig;
use crate::error::FrameworkError;
use crate::provider::{Message, Provider, Role, ToolDefinition};
use crate::tools::{Tool, ToolCtx};

pub async fn run_loop(
    provider: &dyn Provider,
    tool_ctx: &ToolCtx,
    tool_config: &ToolConfig,
    system_prompt: &str,
    session_id: &str,
    mut history: Vec<Message>,
    max_steps: u32,
) -> Result<String, FrameworkError> {
    let tools = tool_definitions(tool_config);

    for _ in 0..max_steps {
        let response = provider.generate(system_prompt, &history, &tools).await?;

        if !response.tool_calls.is_empty() {
            for call in response.tool_calls {
                let observation = match Tool::try_from(call.name.as_str()) {
                    Ok(tool) if tool.is_enabled(tool_config) => {
                        match tool.execute(tool_ctx, &call.args_json, session_id).await {
                            Ok(ok) => ok,
                            Err(err) => format!("tool_error: {err}"),
                        }
                    }
                    Ok(_) => {
                        format!(
                            "tool_error: tool `{}` is disabled for this agent",
                            call.name
                        )
                    }
                    Err(_) => format!("tool_error: unknown tool: {}", call.name),
                };

                history.push(Message {
                    role: Role::Tool,
                    content: format!("{}({}) => {}", call.name, call.args_json, observation),
                });
            }
            continue;
        }

        if let Some(text) = response.output_text {
            history.push(Message {
                role: Role::Assistant,
                content: text.clone(),
            });
            return Ok(text);
        }
    }

    Ok("max_steps reached without final response".to_owned())
}

fn tool_definitions(config: &ToolConfig) -> Vec<ToolDefinition> {
    Tool::all()
        .iter()
        .copied()
        .filter(|tool| tool.is_enabled(config))
        .map(Tool::definition)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_only_include_enabled_tools() {
        let config = ToolConfig {
            memory: true,
            memorize: false,
            summon: true,
            search: false,
            clock: true,
            fetch: false,
            read: true,
            exec: false,
        };

        let names: Vec<String> = tool_definitions(&config)
            .into_iter()
            .map(|tool| tool.name)
            .collect();

        assert_eq!(
            names,
            vec![
                "memory".to_owned(),
                "summon".to_owned(),
                "clock".to_owned(),
                "read".to_owned()
            ]
        );
    }

    #[test]
    fn tool_status_reports_disabled_and_unknown() {
        let config = ToolConfig {
            exec: false,
            ..ToolConfig::default()
        };

        let memory = Tool::try_from("memory").expect("memory should be known");
        assert!(memory.is_enabled(&config));

        let exec = Tool::try_from("exec").expect("exec should be known");
        assert!(!exec.is_enabled(&config));

        assert!(Tool::try_from("not_a_tool").is_err());
    }
}
