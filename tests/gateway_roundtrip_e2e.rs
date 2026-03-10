use rusqlite::Connection;
use serde_json::Value;
use simpleclaw::testing::{
    ProviderScriptStep, ScriptedToolCall, TestAgentConfig, TestHarnessConfig,
    run_single_gateway_roundtrip,
};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn exec_test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn session_rows(db_path: &std::path::Path, session_id: &str) -> Vec<(String, String)> {
    let conn = Connection::open(db_path).expect("short-term sqlite db should be readable");
    let mut stmt = conn
        .prepare(
            "SELECT role, content
             FROM messages
             WHERE session_id = ?1
             ORDER BY id ASC",
        )
        .expect("messages query should prepare");
    stmt.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .expect("messages query should run")
    .collect::<Result<Vec<_>, _>>()
    .expect("rows should decode")
}

#[tokio::test]
async fn gateway_roundtrip_uses_mock_provider_and_persists_ephemeral_sqlite() {
    let config = TestHarnessConfig {
        inbound_content: "ping from e2e".to_owned(),
        mock_reply: "mock-reply".to_owned(),
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 1);
    assert_eq!(result.typing_events, 1);
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(result.outbound_messages[0].content, "mock-reply");

    assert!(result.ephemeral_paths.short_term_db_path.exists());
    assert!(result.ephemeral_paths.long_term_db_path.exists());

    let conn = Connection::open(&result.ephemeral_paths.short_term_db_path)
        .expect("short-term sqlite db should be readable");
    let mut stmt = conn
        .prepare(
            "SELECT role, content
             FROM messages
             WHERE session_id = ?1
             ORDER BY id ASC",
        )
        .expect("messages query should prepare");
    let rows = stmt
        .query_map([result.memory_session_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("messages query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should decode");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "user");
    assert_eq!(rows[0].1, "ping from e2e");
    assert_eq!(rows[1].0, "assistant");
    assert_eq!(rows[1].1, "mock-reply");
}

#[tokio::test]
async fn gateway_roundtrip_suppresses_no_reply_and_skips_assistant_persist() {
    let config = TestHarnessConfig {
        inbound_content: "silent ping".to_owned(),
        mock_reply: "NO_REPLY".to_owned(),
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 1);
    assert_eq!(result.typing_events, 1);
    assert_eq!(result.outbound_messages.len(), 0);

    let conn = Connection::open(&result.ephemeral_paths.short_term_db_path)
        .expect("short-term sqlite db should be readable");
    let mut stmt = conn
        .prepare(
            "SELECT role, content
             FROM messages
             WHERE session_id = ?1
             ORDER BY id ASC",
        )
        .expect("messages query should prepare");
    let rows = stmt
        .query_map([result.memory_session_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("messages query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should decode");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "user");
    assert_eq!(rows[0].1, "silent ping");
}

#[tokio::test]
async fn gateway_listener_roundtrip_routes_and_processes_invoke_message() {
    let config = TestHarnessConfig {
        inbound_content: "ping through listener".to_owned(),
        mock_reply: "listener-reply".to_owned(),
        route_via_gateway_listener: true,
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 1);
    assert_eq!(result.typing_events, 1);
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(result.outbound_messages[0].content, "listener-reply");
    assert_eq!(
        result.memory_session_id,
        "agent:default:discord:integration-channel"
    );

    let conn = Connection::open(&result.ephemeral_paths.short_term_db_path)
        .expect("short-term sqlite db should be readable");
    let mut stmt = conn
        .prepare(
            "SELECT role, content
             FROM messages
             WHERE session_id = ?1
             ORDER BY id ASC",
        )
        .expect("messages query should prepare");
    let rows = stmt
        .query_map([result.memory_session_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("messages query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should decode");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "user");
    assert_eq!(rows[0].1, "ping through listener");
    assert_eq!(rows[1].0, "assistant");
    assert_eq!(rows[1].1, "listener-reply");
}

#[tokio::test]
async fn gateway_listener_roundtrip_routes_context_only_when_mention_missing() {
    let config = TestHarnessConfig {
        inbound_content: "passive listener ping".to_owned(),
        mock_reply: "should-not-run".to_owned(),
        route_via_gateway_listener: true,
        require_mentions: true,
        mentioned_bot: false,
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 0);
    assert_eq!(result.typing_events, 0);
    assert!(result.outbound_messages.is_empty());
    assert_eq!(
        result.memory_session_id,
        "agent:default:discord:integration-channel"
    );

    let conn = Connection::open(&result.ephemeral_paths.short_term_db_path)
        .expect("short-term sqlite db should be readable");
    let mut stmt = conn
        .prepare(
            "SELECT role, content
             FROM messages
             WHERE session_id = ?1
             ORDER BY id ASC",
        )
        .expect("messages query should prepare");
    let rows = stmt
        .query_map([result.memory_session_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("messages query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should decode");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "user");
    assert_eq!(rows[0].1, "passive listener ping");
}

#[tokio::test]
async fn gateway_roundtrip_exec_tool_call_returns_pwd_output() {
    let _guard = exec_test_guard();
    let config = TestHarnessConfig {
        inbound_content: "run pwd".to_owned(),
        mock_reply: "tool-finished".to_owned(),
        scripted_tool_call: Some(ScriptedToolCall {
            id: Some("call-exec-pwd".to_owned()),
            name: "exec".to_owned(),
            args_json: r#"{"command":"pwd"}"#.to_owned(),
        }),
        scripted_final_reply: Some("tool-finished".to_owned()),
        exec_timeout_seconds: Some(10),
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 2);
    assert!(result.observed_tool_result);
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(result.outbound_messages[0].content, "tool-finished");

    let response = result
        .observed_tool_response
        .expect("tool response should be captured");
    assert_eq!(response["status"], Value::String("ok".to_owned()));
    let nested = response["content"]
        .as_str()
        .expect("tool response content should be a string");
    assert!(
        nested.contains("\"status\":\"completed\""),
        "nested={nested}"
    );
    assert!(nested.contains("\"exitCode\":0"), "nested={nested}");
    assert!(nested.contains("\"stdout\":\"/"), "nested={nested}");
}

#[tokio::test]
async fn gateway_roundtrip_exec_tool_call_reports_timeout_error() {
    let _guard = exec_test_guard();
    let config = TestHarnessConfig {
        inbound_content: "run slow command".to_owned(),
        mock_reply: "timeout-finished".to_owned(),
        scripted_tool_call: Some(ScriptedToolCall {
            id: Some("call-exec-timeout".to_owned()),
            name: "exec".to_owned(),
            args_json: r#"{"command":"sleep 3"}"#.to_owned(),
        }),
        scripted_final_reply: Some("timeout-finished".to_owned()),
        exec_timeout_seconds: Some(1),
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 2);
    assert!(result.observed_tool_result);
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(result.outbound_messages[0].content, "timeout-finished");

    let response = result
        .observed_tool_response
        .expect("tool response should be captured");
    assert_eq!(response["status"], Value::String("tool_error".to_owned()));
    let nested = response["content"]
        .as_str()
        .expect("tool response content should be a string");
    assert!(
        nested.contains("exec timed out after 1s in sandbox runtime"),
        "nested={nested}"
    );
}

#[tokio::test]
async fn gateway_roundtrip_exec_tool_call_returns_pwd_output_on_repeated_runs() {
    let _guard = exec_test_guard();
    for _ in 0..2 {
        let config = TestHarnessConfig {
            inbound_content: "run pwd".to_owned(),
            mock_reply: "tool-finished".to_owned(),
            scripted_tool_call: Some(ScriptedToolCall {
                id: Some("call-exec-pwd-repeat".to_owned()),
                name: "exec".to_owned(),
                args_json: r#"{"command":"pwd"}"#.to_owned(),
            }),
            scripted_final_reply: Some("tool-finished".to_owned()),
            exec_timeout_seconds: Some(10),
            ..TestHarnessConfig::default()
        };
        let result = run_single_gateway_roundtrip(config)
            .await
            .expect("integration harness should run");

        let response = result
            .observed_tool_response
            .expect("tool response should be captured");
        assert_eq!(response["status"], Value::String("ok".to_owned()));
        let nested = response["content"]
            .as_str()
            .expect("tool response content should be a string");
        assert!(
            nested.contains("\"status\":\"completed\""),
            "nested={nested}"
        );
        assert!(nested.contains("\"exitCode\":0"), "nested={nested}");
        assert!(nested.contains("\"stdout\":\"/"), "nested={nested}");
    }
}

#[tokio::test]
async fn gateway_listener_roundtrip_sends_safe_error_reply_when_provider_fails() {
    let config = TestHarnessConfig {
        route_via_gateway_listener: true,
        agents: vec![TestAgentConfig::new(
            "default",
            "Default",
            "default",
            vec![ProviderScriptStep::Error(
                "mock provider failure".to_owned(),
            )],
        )],
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert!(result.listener_routed);
    assert_eq!(result.provider_call_count, 1);
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(
        result.outbound_messages[0].content,
        "I hit an internal error while processing that request."
    );

    let rows = session_rows(
        &result.ephemeral_paths.short_term_db_path,
        &result.memory_session_id,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "user");
}

#[tokio::test]
async fn gateway_listener_roundtrip_drops_disallowed_user_before_runtime() {
    let config = TestHarnessConfig {
        route_via_gateway_listener: true,
        allow_from: Some(vec!["allowed-user".to_owned()]),
        user_id: "blocked-user".to_owned(),
        is_dm: true,
        expect_listener_drop: true,
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert!(!result.listener_routed);
    assert_eq!(result.provider_call_count, 0);
    assert!(result.outbound_messages.is_empty());

    let conn = Connection::open(&result.ephemeral_paths.short_term_db_path)
        .expect("short-term sqlite db should be readable");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .expect("message count query should succeed");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn gateway_listener_roundtrip_invokes_dm_without_requiring_mention() {
    let config = TestHarnessConfig {
        route_via_gateway_listener: true,
        is_dm: true,
        require_mentions: true,
        mentioned_bot: false,
        mock_reply: "dm-reply".to_owned(),
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert!(result.listener_routed);
    assert_eq!(result.provider_call_count, 1);
    assert_eq!(result.memory_session_id, "agent:default:main");
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(result.outbound_messages[0].content, "dm-reply");
}

#[tokio::test]
async fn gateway_roundtrip_processes_background_completion_followup() {
    let _guard = exec_test_guard();
    let config = TestHarnessConfig {
        additional_inbounds_to_process: 1,
        additional_inbound_timeout_ms: 4_000,
        agents: vec![
            TestAgentConfig::new(
                "default",
                "Default",
                "default",
                vec![
                    ProviderScriptStep::ToolCall(ScriptedToolCall {
                        id: Some("call-exec-background".to_owned()),
                        name: "exec".to_owned(),
                        args_json: r#"{"command":"sleep 0.2","background":true}"#.to_owned(),
                    }),
                    ProviderScriptStep::Reply("waiting for background".to_owned()),
                    ProviderScriptStep::Reply("background complete".to_owned()),
                ],
            )
            .with_exec_tool(None, true, false),
        ],
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 3);
    assert_eq!(result.outbound_messages.len(), 2);
    assert_eq!(
        result.outbound_messages[0].content,
        "waiting for background"
    );
    assert_eq!(result.outbound_messages[1].content, "background complete");
    let response = result
        .observed_tool_response
        .expect("background tool response should be captured");
    assert_eq!(response["status"], Value::String("ok".to_owned()));
    let nested = response["content"]
        .as_str()
        .expect("tool response content should be a string");
    assert!(
        nested.contains("\"status\":\"backgrounded\""),
        "nested={nested}"
    );
}

#[tokio::test]
async fn gateway_roundtrip_summon_tool_invokes_second_agent() {
    let config = TestHarnessConfig {
        agents: vec![
            TestAgentConfig::new(
                "default",
                "Default",
                "main",
                vec![
                    ProviderScriptStep::ToolCall(ScriptedToolCall {
                        id: Some("call-summon-helper".to_owned()),
                        name: "summon".to_owned(),
                        args_json: r#"{"agent":"helper","summary":"investigate"}"#.to_owned(),
                    }),
                    ProviderScriptStep::Reply("delegated complete".to_owned()),
                ],
            )
            .with_summon_allowed(vec!["helper".to_owned()])
            .with_provider_key("main"),
            TestAgentConfig::new(
                "helper",
                "Helper",
                "helper",
                vec![ProviderScriptStep::Reply("helper result".to_owned())],
            )
            .with_provider_key("helper"),
        ],
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 3);
    assert_eq!(result.provider_call_counts["main"], 2);
    assert_eq!(result.provider_call_counts["helper"], 1);
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(result.outbound_messages[0].content, "delegated complete");
    let response = result
        .observed_tool_response
        .expect("summon tool response should be captured");
    assert_eq!(response["status"], Value::String("ok".to_owned()));
    assert_eq!(
        response["content"],
        Value::String("helper result".to_owned())
    );
}

#[tokio::test]
async fn gateway_roundtrip_task_tool_invokes_worker_flow() {
    let config = TestHarnessConfig {
        agents: vec![
            TestAgentConfig::new(
                "default",
                "Default",
                "default",
                vec![
                    ProviderScriptStep::ToolCall(ScriptedToolCall {
                        id: Some("call-task-worker".to_owned()),
                        name: "task".to_owned(),
                        args_json: r#"{"prompt":"do delegated work"}"#.to_owned(),
                    }),
                    ProviderScriptStep::Reply("worker result".to_owned()),
                    ProviderScriptStep::Reply("task wrapped up".to_owned()),
                ],
            )
            .with_task_worker_max_steps(Some(2)),
        ],
        ..TestHarnessConfig::default()
    };
    let result = run_single_gateway_roundtrip(config)
        .await
        .expect("integration harness should run");

    assert_eq!(result.provider_call_count, 3);
    assert_eq!(result.outbound_messages.len(), 1);
    assert_eq!(result.outbound_messages[0].content, "task wrapped up");
    let response = result
        .observed_tool_response
        .expect("task tool response should be captured");
    assert_eq!(response["status"], Value::String("ok".to_owned()));
    assert_eq!(
        response["content"],
        Value::String("worker result".to_owned())
    );
}
