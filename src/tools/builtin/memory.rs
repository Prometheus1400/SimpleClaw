use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv};

use super::common::{MemoryAction, parse_memory_args};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTool {
    SemanticQuery,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Semantic query long-term memory using JSON: {action?, query, top_k?, kind?, limit?}; kind one of: general|profile|preferences|project|task|constraint"
    }

    fn input_schema_json(&self) -> &'static str {
        r#"{"type":"object","properties":{"action":{"type":"string","enum":["query","list"]},"query":{"type":"string"},"top_k":{"type":"integer"},"kind":{"type":"string","enum":["general","profile","preferences","project","task","constraint"]},"limit":{"type":"integer"}},"required":[]}"#
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        match parse_memory_args(args_json) {
            MemoryAction::Query { query, top_k } => {
                let results = ctx
                    .memory
                    .semantic_query_combined(session_id, &query, top_k)
                    .await?;
                if results.is_empty() {
                    return Ok("no memory hits".to_owned());
                }
                Ok(results
                    .iter()
                    .enumerate()
                    .map(|(i, hit)| format!("{}. {}", i + 1, hit))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            MemoryAction::List { kind, limit } => {
                let facts = ctx
                    .memory
                    .list_long_term_facts(kind.as_deref(), limit)
                    .await?;
                if facts.is_empty() {
                    return Ok("no long-term memories stored".to_owned());
                }
                let lines: Vec<String> = facts
                    .iter()
                    .map(|f| {
                        format!(
                            "[id={} kind={} importance={} at={}] {}",
                            f.id, f.kind, f.importance, f.created_at, f.content
                        )
                    })
                    .collect();
                Ok(format!("{} memories:\n{}", facts.len(), lines.join("\n")))
            }
        }
    }
}
