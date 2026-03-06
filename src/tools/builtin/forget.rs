use async_trait::async_trait;
use serde_json::json;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

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
        "Prune long-term memory by semantic similarity using JSON: {query, commit?, similarity_threshold?, max_matches?, kind?}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"},\"commit\":{\"type\":\"boolean\"},\"similarity_threshold\":{\"type\":\"number\",\"minimum\":0,\"maximum\":1},\"max_matches\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":50},\"kind\":{\"type\":\"string\"}},\"required\":[\"query\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
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

        Ok(json!({
            "status": if args.commit { "deleted" } else { "preview" },
            "query": args.query,
            "similarity_threshold": result.similarity_threshold,
            "max_matches": result.max_matches,
            "kind": result.kind_filter,
            "match_count": result.matches.len(),
            "deleted_count": result.deleted_count,
            "matches": matches,
        })
        .to_string())
    }
}

fn truncate_preview(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let clipped = value.chars().take(max_chars).collect::<String>();
    format!("{clipped}...[truncated]")
}
