mod store;

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use cron::Schedule;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::config::CronToolConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv};

pub(crate) use store::{CronJob, CronStore};

#[derive(Debug, Clone)]
pub(crate) struct CronTool {
    store: Arc<Mutex<CronStore>>,
    config: CronToolConfig,
}

impl CronTool {
    pub(crate) fn new(store: Arc<Mutex<CronStore>>) -> Self {
        Self {
            store,
            config: CronToolConfig::default(),
        }
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, CronStore>, FrameworkError> {
        self.store
            .lock()
            .map_err(|_| FrameworkError::Tool("cron store lock poisoned".to_owned()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CronAction {
    Create,
    Delete,
    List,
}

#[derive(Debug, Deserialize)]
struct CronArgs {
    action: CronAction,
    schedule: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    guard_command: Option<String>,
    id: Option<String>,
    query: Option<String>,
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &'static str {
        "cron"
    }

    fn description(&self) -> &'static str {
        "Manage agent cron jobs using JSON: {action:create|delete|list, schedule?, description?, prompt?, guard_command?, id?, query?}. Create requires schedule, description, and prompt."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"action\":{\"type\":\"string\",\"enum\":[\"create\",\"delete\",\"list\"]},\"schedule\":{\"type\":\"string\"},\"description\":{\"type\":\"string\"},\"prompt\":{\"type\":\"string\"},\"guard_command\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"query\":{\"type\":\"string\"}},\"required\":[\"action\"]}"
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.cron config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let args: CronArgs = serde_json::from_str(args_json).map_err(|e| {
            FrameworkError::Tool(format!("cron args must be valid JSON object: {e}"))
        })?;

        match args.action {
            CronAction::Create => {
                let route = ctx.completion_route.as_ref().ok_or_else(|| {
                    FrameworkError::Tool(
                        "cron create unavailable: completion route context is missing".to_owned(),
                    )
                })?;
                let schedule = args
                    .schedule
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        FrameworkError::Tool("cron create requires non-empty schedule".to_owned())
                    })?
                    .to_owned();
                parse_schedule(&schedule).map_err(|err| {
                    FrameworkError::Tool(format!("invalid cron schedule '{schedule}': {err}"))
                })?;

                let description = args
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        FrameworkError::Tool(
                            "cron create requires non-empty description".to_owned(),
                        )
                    })?
                    .to_owned();

                let prompt = args
                    .prompt
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        FrameworkError::Tool("cron create requires non-empty prompt".to_owned())
                    })?
                    .to_owned();

                let max_jobs = self.config.max_jobs_per_agent.unwrap_or(50);
                let store = self.lock_store()?;
                let current_count = store.count_jobs_for_agent(&ctx.agent_id)?;
                if current_count >= max_jobs {
                    return Err(FrameworkError::Tool(format!(
                        "cron create rejected: max jobs per agent ({max_jobs}) reached"
                    )));
                }

                let guard_command = args
                    .guard_command
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                let guard_timeout_seconds = self.config.guard_timeout_seconds.unwrap_or(10).max(1);

                let job = CronJob {
                    id: Uuid::new_v4().to_string(),
                    agent_id: ctx.agent_id.clone(),
                    schedule,
                    description,
                    prompt,
                    guard_command,
                    workspace_root: ctx.workspace_root.display().to_string(),
                    channel_id: route.channel_id.clone(),
                    guild_id: route.guild_id.clone(),
                    source_channel: route.source_channel.as_str().to_owned(),
                    is_dm: route.is_dm,
                    created_by: ctx.user_id.clone(),
                    created_at: Utc::now(),
                    last_fired_at: None,
                    guard_timeout_seconds,
                    enabled: true,
                };

                store.create_job(&job)?;

                Ok(json!({
                    "status": "created",
                    "id": job.id,
                    "schedule": job.schedule,
                    "description": job.description,
                    "prompt": job.prompt,
                    "guardCommand": job.guard_command,
                    "guardTimeoutSeconds": job.guard_timeout_seconds
                })
                .to_string())
            }
            CronAction::Delete => {
                let id = args
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        FrameworkError::Tool("cron delete requires non-empty id".to_owned())
                    })?;
                let store = self.lock_store()?;
                let deleted = store.delete_job(id, &ctx.agent_id)?;
                Ok(json!({"status":"ok","deleted":deleted,"id":id}).to_string())
            }
            CronAction::List => {
                let query = args
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                let store = self.lock_store()?;
                let jobs = store.list_jobs(&ctx.agent_id, query.as_deref())?;
                let jobs_json: Vec<serde_json::Value> = jobs.iter().map(job_to_json).collect();
                Ok(json!({"status":"ok","jobs":jobs_json}).to_string())
            }
        }
    }
}

fn job_to_json(job: &CronJob) -> serde_json::Value {
    json!({
        "id": job.id,
        "agentId": job.agent_id,
        "schedule": job.schedule,
        "description": job.description,
        "prompt": job.prompt,
        "guardCommand": job.guard_command,
        "workspaceRoot": job.workspace_root,
        "channelId": job.channel_id,
        "guildId": job.guild_id,
        "sourceChannel": job.source_channel,
        "isDm": job.is_dm,
        "createdBy": job.created_by,
        "createdAt": job.created_at.to_rfc3339(),
        "lastFiredAt": job.last_fired_at.map(|dt| dt.to_rfc3339()),
        "guardTimeoutSeconds": job.guard_timeout_seconds,
        "enabled": job.enabled,
    })
}

fn parse_schedule(raw: &str) -> Result<Schedule, cron::error::Error> {
    match Schedule::from_str(raw) {
        Ok(schedule) => Ok(schedule),
        Err(err) => {
            if raw.split_whitespace().count() == 5 {
                Schedule::from_str(&format!("0 {raw}"))
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{CronStore, CronTool};
    use crate::tools::Tool;
    use uuid::Uuid;

    #[test]
    fn input_schema_avoids_conditional_keywords() {
        let path = std::env::temp_dir().join(format!("simpleclaw-cron-schema-{}.db", Uuid::new_v4()));
        let tool = CronTool::new(Arc::new(Mutex::new(
            CronStore::open(&path).expect("cron store"),
        )));
        let schema = tool.input_schema_json();

        assert!(!schema.contains("\"allOf\""));
        assert!(!schema.contains("\"if\""));
        assert!(!schema.contains("\"then\""));

        let _ = std::fs::remove_file(path);
    }
}
