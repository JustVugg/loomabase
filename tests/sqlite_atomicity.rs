use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::client::{
    SqliteClient, acknowledge_local_delta, acknowledge_local_delta_with_response,
    apply_remote_payload, get_local_delta, initialize_client,
};
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, MAX_CLOCK_ADVANCE_PER_SYNC, PROTOCOL_VERSION, RowChange,
    SERVER_DEVICE_ID, SyncPayload, SyncRejection, SyncRejectionKind,
};
use loomabase::schema::todos_table;
use rusqlite::{Connection, TransactionBehavior, params};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn sqlite_client_is_send_and_sync() {
    assert_send_sync::<SqliteClient>();
}

#[tokio::test]
async fn async_sqlite_facade_executes_on_tokio_blocking_pool() -> Result<()> {
    let client = SqliteClient::open(":memory:", "device-a").await?;
    client
        .create_todo("todo-1".to_owned(), "baseline".to_owned(), false)
        .await?;
    client
        .update_title("todo-1".to_owned(), "updated".to_owned())
        .await?;
    let delta = client.local_delta().await?;
    assert_eq!(delta.changes.len(), 1);
    assert_eq!(delta.source_device_id, "device-a");
    Ok(())
}

#[tokio::test]
async fn sync_with_rolls_back_acknowledgement_when_response_is_invalid() -> Result<()> {
    let client = SqliteClient::open(":memory:", "device-a").await?;
    client
        .create_todo("todo-1".to_owned(), "retryable".to_owned(), false)
        .await?;
    let invalid_response = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "loomabase-server".to_owned(),
        source_lamport: 2,
        changes: vec![RowChange {
            todo_id: "todo-1".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Boolean(true),
                    metadata: ColumnMetadata {
                        lamport_clock: 2,
                        device_id: "device-b".to_owned(),
                    },
                },
            )]),
        }],
        cursor: 0,
        has_more: false,
        cursor_reset: false,
        cursor_token: None,
        server_epoch: None,
        rejections: Vec::new(),
    };

    assert!(
        client
            .sync_with(|_| async { Ok(invalid_response) })
            .await
            .is_err()
    );
    assert!(!client.local_delta().await?.changes.is_empty());
    Ok(())
}

#[tokio::test]
async fn local_api_rejects_values_that_the_server_would_reject() -> Result<()> {
    use loomabase::crdt::MAX_TEXT_BYTES;

    let client = SqliteClient::open(":memory:", "device-a").await?;
    let oversized = "x".repeat(MAX_TEXT_BYTES + 1);
    assert!(
        client
            .create_todo("todo-1".to_owned(), oversized, false)
            .await
            .is_err()
    );
    assert!(client.get_todo("todo-1".to_owned()).await?.is_none());
    Ok(())
}

#[test]
fn acknowledgement_rejects_a_payload_from_another_device() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "device-a")?;
    connection.execute(
        "INSERT INTO todos(id, title, completed) VALUES (?1, ?2, ?3)",
        params!["todo-1", "baseline", false],
    )?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut payload = get_local_delta(&tx, &todos_table())?;
    tx.commit()?;
    payload.source_device_id = "device-b".to_owned();
    payload.changes[0]
        .columns
        .values_mut()
        .for_each(|column| column.metadata.device_id = "device-b".to_owned());

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    assert!(acknowledge_local_delta(&tx, &payload, &todos_table()).is_err());
    drop(tx);

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    assert!(!get_local_delta(&tx, &todos_table())?.changes.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn acknowledgement_does_not_clear_a_concurrent_local_write() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "device-a")?;
    connection.execute(
        "INSERT INTO todos(id, title, completed) VALUES (?1, ?2, ?3)",
        params!["todo-1", "baseline", false],
    )?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sent = get_local_delta(&tx, &todos_table())?;
    tx.commit()?;

    connection.execute(
        "UPDATE todos SET title = ?2 WHERE id = ?1",
        params!["todo-1", "newer local write"],
    )?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    acknowledge_local_delta(&tx, &sent, &todos_table())?;
    tx.commit()?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let next = get_local_delta(&tx, &todos_table())?;
    tx.commit()?;
    assert_eq!(
        next.changes[0].columns["title"].value,
        CrdtValue::Text("newer local write".to_owned())
    );
    Ok(())
}

#[test]
fn rejected_cells_remain_dirty_after_filtered_acknowledgement() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "device-a")?;
    connection.execute(
        "INSERT INTO todos(id, title, completed) VALUES (?1, ?2, ?3)",
        params!["todo-1", "local title", false],
    )?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sent = get_local_delta(&tx, &todos_table())?;
    tx.commit()?;
    let title = sent.changes[0].columns["title"].clone();
    let response = SyncPayload {
        protocol_version: sent.protocol_version,
        schema_fingerprint: sent.schema_fingerprint,
        source_device_id: SERVER_DEVICE_ID.to_owned(),
        source_lamport: sent.source_lamport + 1,
        changes: Vec::new(),
        cursor: 0,
        has_more: false,
        cursor_reset: false,
        cursor_token: None,
        server_epoch: None,
        rejections: vec![SyncRejection::new(
            "todo-1",
            "title",
            SyncRejectionKind::ValidationFailed,
            "title is not valid for this workspace",
            title.value,
            title.metadata,
        )?],
    };

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    acknowledge_local_delta_with_response(&tx, &sent, &response, &todos_table())?;
    tx.commit()?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let retry = get_local_delta(&tx, &todos_table())?;
    tx.commit()?;
    assert_eq!(retry.changes.len(), 1);
    assert!(retry.changes[0].columns.contains_key("title"));
    Ok(())
}

#[test]
fn invalid_remote_payload_rolls_back_trigger_suspension_and_data() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "device-a")?;
    let invalid = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "loomabase-server".to_owned(),
        source_lamport: 2,
        changes: vec![RowChange {
            todo_id: "todo-1".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Boolean(true),
                    metadata: ColumnMetadata {
                        lamport_clock: 2,
                        device_id: "device-b".to_owned(),
                    },
                },
            )]),
        }],
        cursor: 0,
        has_more: false,
        cursor_reset: false,
        cursor_token: None,
        server_epoch: None,
        rejections: Vec::new(),
    };

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    assert!(apply_remote_payload(&tx, &invalid, &todos_table()).is_err());
    drop(tx);

    let applying_remote: i64 = connection.query_row(
        "SELECT applying_remote FROM client_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let todo_count: i64 =
        connection.query_row("SELECT COUNT(*) FROM todos", [], |row| row.get(0))?;
    assert_eq!(applying_remote, 0);
    assert_eq!(todo_count, 0);
    Ok(())
}

#[test]
fn triggers_increment_clock_and_capture_only_changed_column() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "device-a")?;
    connection.execute(
        "INSERT INTO todos(id, title, completed) VALUES (?1, ?2, ?3)",
        params!["todo-1", "baseline", false],
    )?;

    let initial_clock: i64 = connection.query_row(
        "SELECT lamport_clock FROM client_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    connection.execute(
        "UPDATE todos SET completed = TRUE WHERE id = ?1",
        ["todo-1"],
    )?;
    let final_clock: i64 = connection.query_row(
        "SELECT lamport_clock FROM client_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let completed_clock: i64 = connection.query_row(
        "SELECT lamport_clock FROM todos_crdt
         WHERE todo_id = ?1 AND column_name = 'completed'",
        ["todo-1"],
        |row| row.get(0),
    )?;
    let title_clock: i64 = connection.query_row(
        "SELECT lamport_clock FROM todos_crdt
         WHERE todo_id = ?1 AND column_name = 'title'",
        ["todo-1"],
        |row| row.get(0),
    )?;

    assert_eq!(final_clock, initial_clock + 1);
    assert_eq!(completed_clock, final_clock);
    assert_eq!(title_clock, initial_clock);
    Ok(())
}

#[test]
fn new_client_accepts_a_far_advanced_trusted_server_clock() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "device-a")?;
    let server_clock = MAX_CLOCK_ADVANCE_PER_SYNC + 50;
    let response = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: SERVER_DEVICE_ID.to_owned(),
        source_lamport: server_clock,
        changes: Vec::new(),
        cursor: 0,
        has_more: false,
        cursor_reset: false,
        cursor_token: None,
        server_epoch: None,
        rejections: Vec::new(),
    };

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    apply_remote_payload(&tx, &response, &todos_table())?;
    tx.commit()?;
    let stored_clock: i64 = connection.query_row(
        "SELECT lamport_clock FROM client_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(stored_clock, i64::try_from(server_clock + 1).unwrap());
    Ok(())
}

#[tokio::test]
async fn client_persists_opaque_cursor_capability_and_server_epoch() -> Result<()> {
    let client = SqliteClient::open(":memory:", "device-a").await?;
    let mut response = SyncPayload::empty(SERVER_DEVICE_ID, 1, &todos_table());
    response.cursor = 42;
    response.cursor_token = Some("opaque-token".to_owned());
    response.server_epoch = Some("epoch-one".to_owned());
    client.apply_remote(response).await?;

    let next = client.local_delta().await?;
    assert_eq!(next.cursor, 42);
    assert_eq!(next.cursor_token.as_deref(), Some("opaque-token"));
    assert_eq!(next.server_epoch.as_deref(), Some("epoch-one"));

    client.reset_cursor().await?;
    let reset = client.local_delta().await?;
    assert_eq!(reset.cursor, 0);
    assert!(reset.cursor_token.is_none());
    assert!(reset.server_epoch.is_none());
    Ok(())
}
