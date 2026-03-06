use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

use super::common::parse_memory_args;

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
        "Semantic query short-term + long-term memory using JSON: {query, top_k?}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"},\"top_k\":{\"type\":\"integer\"}},\"required\":[\"query\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (query, top_k) = parse_memory_args(args_json);
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
}
