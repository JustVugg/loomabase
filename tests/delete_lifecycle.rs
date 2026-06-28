use std::collections::BTreeMap;
use std::sync::Arc;

use loomabase::Result;
use loomabase::client::SqliteClient;
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtState, CrdtValue, DELETED_COLUMN, PROTOCOL_VERSION, RowChange,
    SyncPayload,
};
use loomabase::schema::todos_table;
use tokio::sync::Mutex;

async fn sync(client: &SqliteClient, server: &Arc<Mutex<CrdtState>>) -> Result<()> {
    let server = Arc::clone(server);
    client
        .sync_with(move |payload| async move {
            let device_id = payload.source_device_id.clone();
            server.lock().await.merge(payload, &device_id)
        })
        .await?;
    Ok(())
}

/// Converges two devices through the in-memory reference server.
async fn converge(
    a: &SqliteClient,
    b: &SqliteClient,
    server: &Arc<Mutex<CrdtState>>,
) -> Result<()> {
    sync(a, server).await?;
    sync(b, server).await?;
    sync(a, server).await?;
    sync(b, server).await?;
    Ok(())
}

fn liveness_payload(device_id: &str, clock: u64, deleted: bool) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.to_owned(),
        source_lamport: clock,
        changes: vec![RowChange {
            todo_id: "todo-1".to_owned(),
            columns: BTreeMap::from([(
                DELETED_COLUMN.to_owned(),
                CrdtColumn {
                    value: CrdtValue::Boolean(deleted),
                    metadata: ColumnMetadata {
                        lamport_clock: clock,
                        device_id: device_id.to_owned(),
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
    }
}

#[tokio::test]
async fn delete_propagates_to_another_device() -> Result<()> {
    let a = SqliteClient::open(":memory:", "device-a").await?;
    let b = SqliteClient::open(":memory:", "device-b").await?;
    let server = Arc::new(Mutex::new(CrdtState::default()));

    a.create_todo("todo-1".into(), "task".into(), false).await?;
    converge(&a, &b, &server).await?;
    assert!(b.get_todo("todo-1".into()).await?.is_some());

    a.delete_todo("todo-1".into()).await?;
    converge(&a, &b, &server).await?;

    assert!(a.get_todo("todo-1".into()).await?.is_none());
    assert!(b.get_todo("todo-1".into()).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn restore_after_delete_brings_the_row_back() -> Result<()> {
    let a = SqliteClient::open(":memory:", "device-a").await?;
    let b = SqliteClient::open(":memory:", "device-b").await?;
    let server = Arc::new(Mutex::new(CrdtState::default()));

    a.create_todo("todo-1".into(), "task".into(), false).await?;
    a.delete_todo("todo-1".into()).await?;
    converge(&a, &b, &server).await?;
    assert!(b.get_todo("todo-1".into()).await?.is_none());

    a.restore_todo("todo-1".into()).await?;
    converge(&a, &b, &server).await?;

    let restored = b.get_todo("todo-1".into()).await?;
    assert_eq!(restored.map(|todo| todo.title), Some("task".to_owned()));
    Ok(())
}

#[tokio::test]
async fn concurrent_edit_is_preserved_through_delete_and_restore() -> Result<()> {
    let a = SqliteClient::open(":memory:", "device-a").await?;
    let b = SqliteClient::open(":memory:", "device-b").await?;
    let server = Arc::new(Mutex::new(CrdtState::default()));

    a.create_todo("todo-1".into(), "task".into(), false).await?;
    converge(&a, &b, &server).await?;

    // Offline: A deletes the row while B edits an unrelated column.
    a.delete_todo("todo-1".into()).await?;
    b.update_title("todo-1".into(), "edited on B".into())
        .await?;
    converge(&a, &b, &server).await?;

    // Liveness is its own register, so the concurrent edit does not resurrect.
    assert!(a.get_todo("todo-1".into()).await?.is_none());
    assert!(b.get_todo("todo-1".into()).await?.is_none());

    // The restore wins over the tombstone and reveals the preserved edit.
    a.restore_todo("todo-1".into()).await?;
    converge(&a, &b, &server).await?;

    assert_eq!(
        a.get_todo("todo-1".into()).await?.map(|todo| todo.title),
        Some("edited on B".to_owned())
    );
    assert_eq!(
        b.get_todo("todo-1".into()).await?.map(|todo| todo.title),
        Some("edited on B".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn local_lifecycle_hides_and_reveals_the_row() -> Result<()> {
    let client = SqliteClient::open(":memory:", "device-a").await?;
    client
        .create_todo("todo-1".into(), "task".into(), false)
        .await?;
    assert!(client.get_todo("todo-1".into()).await?.is_some());

    client.delete_todo("todo-1".into()).await?;
    assert!(client.get_todo("todo-1".into()).await?.is_none());

    client.restore_todo("todo-1".into()).await?;
    assert!(client.get_todo("todo-1".into()).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn delete_and_restore_reject_invalid_lifecycle_transitions() -> Result<()> {
    let client = SqliteClient::open(":memory:", "device-a").await?;
    assert!(client.delete_todo("missing".into()).await.is_err());

    client
        .create_todo("todo-1".into(), "task".into(), false)
        .await?;
    assert!(client.restore_todo("todo-1".into()).await.is_err());

    client.delete_todo("todo-1".into()).await?;
    assert!(client.delete_todo("todo-1".into()).await.is_err());
    assert!(client.restore_todo("todo-1".into()).await.is_ok());
    Ok(())
}

#[test]
fn liveness_register_is_last_writer_wins() -> Result<()> {
    let mut server = CrdtState::default();
    server.merge(liveness_payload("device-a", 5, true), "device-a")?;
    server.merge(liveness_payload("device-a", 9, false), "device-a")?;
    assert_eq!(
        server.cells[&("todo-1".to_owned(), DELETED_COLUMN.to_owned())].value,
        CrdtValue::Boolean(false)
    );

    let mut reverse = CrdtState::default();
    reverse.merge(liveness_payload("device-a", 5, false), "device-a")?;
    reverse.merge(liveness_payload("device-a", 9, true), "device-a")?;
    assert_eq!(
        reverse.cells[&("todo-1".to_owned(), DELETED_COLUMN.to_owned())].value,
        CrdtValue::Boolean(true)
    );
    Ok(())
}

#[test]
fn delete_merge_is_idempotent() -> Result<()> {
    let mut server = CrdtState::default();
    server.merge(liveness_payload("device-a", 4, true), "device-a")?;
    let after_first = server.clone();
    server.merge(liveness_payload("device-a", 4, true), "device-a")?;
    assert_eq!(server.cells, after_first.cells);
    Ok(())
}
