use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration as ChronoDuration, Timelike, Utc};
use cron::Schedule;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, interval};
use tracing::{debug, warn};

use crate::channels::{InboundMessage, InboundMessageKind};
use crate::config::GatewayChannelKind;
use crate::sandbox::{DefaultHostSandbox, HostSandbox, RunHostCommandRequest, SandboxPolicy};
use crate::telemetry::next_trace_id;
use crate::tools::builtin::cron::{CronJob, CronStore};

pub fn spawn(
    store: Arc<Mutex<CronStore>>,
    gateway_tx: mpsc::Sender<InboundMessage>,
    env_by_agent: BTreeMap<String, BTreeMap<String, String>>,
    default_guard_timeout: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            run_tick(&store, &gateway_tx, &env_by_agent, default_guard_timeout).await;
        }
    })
}

async fn run_tick(
    store: &Arc<Mutex<CronStore>>,
    gateway_tx: &mpsc::Sender<InboundMessage>,
    env_by_agent: &BTreeMap<String, BTreeMap<String, String>>,
    default_guard_timeout: u64,
) {
    let jobs = {
        let guard = match store.lock() {
            Ok(guard) => guard,
            Err(_) => {
                warn!(
                    status = "failed",
                    error_kind = "cron_store_lock",
                    "cron scheduler"
                );
                return;
            }
        };
        match guard.list_all_enabled() {
            Ok(jobs) => jobs,
            Err(err) => {
                warn!(status = "failed", error_kind = "cron_store_list", error = %err, "cron scheduler");
                return;
            }
        }
    };

    let now = Utc::now();
    for job in jobs {
        if !should_fire(&job, now) {
            continue;
        }

        if let Some(guard_command) = job.guard_command.as_deref()
            && !guard_command.trim().is_empty()
        {
            let timeout_secs = if job.guard_timeout_seconds == 0 {
                default_guard_timeout.max(1)
            } else {
                job.guard_timeout_seconds
            };
            let env = env_by_agent.get(&job.agent_id).cloned().unwrap_or_default();
            match run_guard_command(
                guard_command,
                Path::new(&job.workspace_root),
                &env,
                timeout_secs,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => {
                    debug!(status = "skipped", job_id = %job.id, reason = "guard_exit_nonzero", "cron scheduler");
                    continue;
                }
                Err(err) => {
                    warn!(status = "failed", job_id = %job.id, error_kind = "guard_exec", error = %err, "cron scheduler");
                    continue;
                }
            }
        }

        let Some(source_channel) = parse_source_channel(&job.source_channel) else {
            warn!(
                status = "failed",
                job_id = %job.id,
                source_channel = %job.source_channel,
                error_kind = "unknown_source_channel",
                "cron scheduler"
            );
            continue;
        };

        let inbound = InboundMessage {
            trace_id: next_trace_id(),
            source_channel,
            target_agent_id: job.agent_id.clone(),
            session_key: String::new(),
            source_message_id: None,
            channel_id: job.channel_id.clone(),
            guild_id: job.guild_id.clone(),
            is_dm: job.is_dm,
            user_id: "system".to_owned(),
            username: "cron".to_owned(),
            mentioned_bot: false,
            invoke: true,
            content: String::new(),
            kind: InboundMessageKind::Text,
        };
        let trace_id = inbound.trace_id.clone();
        let inbound = InboundMessage {
            session_key: format!("agent:{}:cron:{}:{trace_id}", job.agent_id, job.id),
            content: format!(
                "[SCHEDULED CRON EVENT]\nJob ID: {}\nDescription: {}\nSchedule: {}\n\n{}",
                job.id, job.description, job.schedule, job.prompt
            ),
            ..inbound
        };

        if let Err(err) = gateway_tx.send(inbound).await {
            warn!(
                status = "failed",
                job_id = %job.id,
                error_kind = "gateway_send",
                error = %err,
                "cron scheduler"
            );
            continue;
        }

        if let Ok(guard) = store.lock() {
            if let Err(err) = guard.update_last_fired(&job.id, now) {
                warn!(
                    status = "failed",
                    job_id = %job.id,
                    error_kind = "cron_store_update_last_fired",
                    error = %err,
                    "cron scheduler"
                );
            }
        } else {
            warn!(
                status = "failed",
                error_kind = "cron_store_lock",
                "cron scheduler"
            );
        }
    }
}

fn should_fire(job: &CronJob, now: DateTime<Utc>) -> bool {
    let schedule = match parse_schedule(&job.schedule) {
        Ok(schedule) => schedule,
        Err(err) => {
            warn!(
                status = "failed",
                job_id = %job.id,
                schedule = %job.schedule,
                error_kind = "invalid_schedule",
                error = %err,
                "cron scheduler"
            );
            return false;
        }
    };

    if let Some(last_fired_at) = job.last_fired_at {
        return schedule
            .after(&last_fired_at)
            .take_while(|next| *next <= now)
            .next()
            .is_some();
    }

    let Some(minute_start) = now
        .with_second(0)
        .and_then(|value| value.with_nanosecond(0))
    else {
        return false;
    };
    let minute_start_exclusive = minute_start - ChronoDuration::seconds(1);
    schedule
        .after(&minute_start_exclusive)
        .next()
        .map(|next| next >= minute_start && next <= now)
        .unwrap_or(false)
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

async fn run_guard_command(
    command: &str,
    workspace_root: &Path,
    env: &BTreeMap<String, String>,
    timeout_seconds: u64,
) -> Result<bool, crate::error::FrameworkError> {
    let sandbox_cfg = SandboxPolicy {
        extra_writable_paths: Vec::new(),
        network_enabled: false,
    };

    let output = DefaultHostSandbox
        .run(RunHostCommandRequest {
            command: command.to_owned(),
            workspace_root: workspace_root.to_path_buf(),
            policy: sandbox_cfg,
            env: env.clone(),
            timeout_seconds,
        })
        .await?;

    Ok(output.exit_code == 0)
}

fn parse_source_channel(raw: &str) -> Option<GatewayChannelKind> {
    match raw.trim() {
        "discord" => Some(GatewayChannelKind::Discord),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use chrono::{Duration as ChronoDuration, Timelike, Utc};

    use crate::tools::builtin::cron::CronJob;

    use super::{run_guard_command, should_fire};

    fn sample_job(schedule: &str, last_fired_at: Option<chrono::DateTime<Utc>>) -> CronJob {
        CronJob {
            id: "job-1".to_owned(),
            agent_id: "agent-a".to_owned(),
            schedule: schedule.to_owned(),
            description: "sample cron job".to_owned(),
            prompt: "run".to_owned(),
            guard_command: None,
            workspace_root: "/tmp".to_owned(),
            channel_id: "channel-1".to_owned(),
            guild_id: None,
            source_channel: "discord".to_owned(),
            is_dm: false,
            created_by: "owner".to_owned(),
            created_at: Utc::now(),
            last_fired_at,
            guard_timeout_seconds: 10,
            enabled: true,
        }
    }

    #[test]
    fn should_fire_when_interval_elapsed_since_last_fired() {
        let now = Utc::now()
            .with_second(30)
            .and_then(|value| value.with_nanosecond(0))
            .expect("timestamp should align to deterministic second");
        let last = now - ChronoDuration::minutes(2);
        let job = sample_job("*/1 * * * *", Some(last));

        assert!(should_fire(&job, now));
    }

    #[test]
    fn should_not_fire_when_already_fired_this_minute() {
        let now = Utc::now()
            .with_second(30)
            .and_then(|value| value.with_nanosecond(0))
            .expect("timestamp should align to deterministic second");
        let last = now
            .with_second(10)
            .expect("timestamp should align to deterministic second");
        let job = sample_job("*/1 * * * *", Some(last));

        assert!(!should_fire(&job, now));
    }

    #[test]
    fn should_fire_without_last_fired_when_schedule_matches_current_minute() {
        let aligned = Utc::now()
            .with_second(0)
            .and_then(|value| value.with_nanosecond(0))
            .expect("timestamp should align to minute");
        let job = sample_job("* * * * *", None);

        assert!(should_fire(&job, aligned + ChronoDuration::seconds(10)));
    }

    // Manual test: this exercises the real sandboxed cron guard command path
    // and must be run outside the Codex sandbox.
    #[tokio::test]
    #[ignore = "must be run outside the Codex sandbox"]
    async fn guard_command_receives_execution_env() {
        let nanos = Utc::now()
            .timestamp_nanos_opt()
            .expect("nanos should exist");
        let workspace = std::env::temp_dir().join(format!("simpleclaw_cron_env_{nanos}"));
        fs::create_dir_all(&workspace).expect("workspace should exist");
        let output_path = workspace.join("guard-env.txt");
        let command = format!(
            "test \"$SIMPLECLAW_CRON_ENV\" = cron-secret && printf %s \"$SIMPLECLAW_CRON_ENV\" > {}",
            output_path.display()
        );

        let ok = run_guard_command(
            &command,
            &workspace,
            &BTreeMap::from([("SIMPLECLAW_CRON_ENV".to_owned(), "cron-secret".to_owned())]),
            10,
        )
        .await
        .expect("guard command should run");

        assert!(ok);
        assert_eq!(
            fs::read_to_string(&output_path).expect("guard output should exist"),
            "cron-secret"
        );

        let _ = fs::remove_dir_all(&workspace);
    }
}
