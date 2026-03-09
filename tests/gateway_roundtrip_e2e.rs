use rusqlite::Connection;
#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(target_os = "linux")]
use simpleclaw::testing::ScriptedToolCall;
use simpleclaw::testing::{TestHarnessConfig, run_single_gateway_roundtrip};

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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn gateway_roundtrip_exec_tool_call_returns_pwd_output() {
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn gateway_roundtrip_exec_tool_call_reports_timeout_error() {
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn gateway_roundtrip_exec_tool_call_returns_pwd_output_on_repeated_runs() {
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
