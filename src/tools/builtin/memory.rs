use async_trait::async_trait;

use crate::config::MemoryToolConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv};

use super::common::{MemoryAction, parse_memory_args};

const DEFAULT_TOP_K: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryTool {
    config: MemoryToolConfig,
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

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.memory config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        match parse_memory_args(args_json) {
            MemoryAction::Query { query, top_k } => {
                let top_k = self.resolve_top_k(top_k)?;
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

impl MemoryTool {
    fn resolve_top_k(&self, requested: Option<usize>) -> Result<usize, FrameworkError> {
        let mut effective = requested
            .or(self.config.default_top_k.map(|v| v as usize))
            .unwrap_or(DEFAULT_TOP_K);
        if let Some(max_top_k) = self.config.max_top_k.map(|v| v as usize) {
            effective = effective.min(max_top_k);
        }
        if effective == 0 {
            return Err(FrameworkError::Tool(
                "memory top_k resolved to 0; configure tools.memory.default_top_k/max_top_k to positive values".to_owned(),
            ));
        }
        Ok(effective)
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryTool;

    #[test]
    fn resolve_top_k_uses_default_when_missing() {
        let mut tool = MemoryTool::default();
        tool.config.default_top_k = Some(7);
        assert_eq!(tool.resolve_top_k(None).expect("should resolve"), 7);
    }

    #[test]
    fn resolve_top_k_clamps_to_max() {
        let mut tool = MemoryTool::default();
        tool.config.max_top_k = Some(3);
        assert_eq!(tool.resolve_top_k(Some(10)).expect("should resolve"), 3);
    }

    #[test]
    fn resolve_top_k_rejects_zero_after_clamp() {
        let mut tool = MemoryTool::default();
        tool.config.default_top_k = Some(0);
        assert!(tool.resolve_top_k(None).is_err());
    }
}
