use rusqlite::Connection;
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
