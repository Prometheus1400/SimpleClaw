use async_trait::async_trait;
use serde_json::json;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, ToolExecutionOutcome};

use super::common::parse_forget_args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetTool {
    LongTermSemanticPrune,
}

#[async_trait]
impl Tool for ForgetTool {
    fn name(&self) -> &'static str {
        "forget"
    }

    fn description(&self) -> &'static str {
        "Remove long-term facts by semantic similarity. Call with commit=false (default) to preview matches without deleting. Call with commit=true to permanently delete."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\",\"description\":\"Natural-language query used to find matching long-term facts.\"},\"commit\":{\"type\":\"boolean\",\"description\":\"Set true to permanently delete matches. Defaults to false for preview mode.\"},\"similarity_threshold\":{\"type\":\"number\",\"minimum\":0,\"maximum\":1,\"description\":\"Minimum semantic similarity required to match a fact.\"},\"max_matches\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":50,\"description\":\"Maximum number of matching facts to return or delete.\"},\"kind\":{\"type\":\"string\",\"enum\":[\"general\",\"profile\",\"preferences\",\"project\",\"task\",\"constraint\"],\"description\":\"Restrict matching to a specific fact category.\"}},\"required\":[\"query\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        _session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let args = parse_forget_args(args_json);
        let result = ctx
            .memory
            .semantic_forget_long_term(
                &args.query,
                args.similarity_threshold,
                args.max_matches,
                args.kind.as_deref(),
                args.commit,
            )
            .await?;

        let matches = result
            .matches
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "kind": m.kind,
                    "importance": m.importance,
                    "similarity": m.similarity,
                    "content_preview": truncate_preview(&m.content, 180),
                })
            })
            .collect::<Vec<_>>();

        Ok(ToolExecutionOutcome::completed(
            json!({
                "status": if args.commit { "deleted" } else { "preview" },
                "query": args.query,
                "similarity_threshold": result.similarity_threshold,
                "max_matches": result.max_matches,
                "kind": result.kind_filter,
                "match_count": result.matches.len(),
                "deleted_count": result.deleted_count,
                "matches": matches,
            })
            .to_string(),
        ))
    }
}

fn truncate_preview(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let clipped = value.chars().take(max_chars).collect::<String>();
    format!("{clipped}...[truncated]")
}
