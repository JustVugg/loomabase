use std::collections::BTreeMap;
use std::time::Duration;

use loomabase::Result;
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;
use loomabase::server::{expire_cursor_leases, initialize_server_schema, merge_crdt_states};
use sqlx_postgres::PgPool;

fn push(device_id: &str, todo_id: &str, title: &str, lamport: u64) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.to_owned(),
        source_lamport: lamport,
        changes: vec![RowChange {
            todo_id: todo_id.to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text(title.to_owned()),
                    metadata: ColumnMetadata {
                        lamport_clock: lamport,
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

fn pull(device_id: &str, lamport: u64, cursor: i64) -> SyncPayload {
    let mut payload = SyncPayload::empty(device_id, lamport, &todos_table());
    payload.cursor = cursor;
    payload
}

fn continue_pull(device_id: &str, lamport: u64, previous: &SyncPayload) -> SyncPayload {
    let mut payload = pull(device_id, lamport, previous.cursor);
    payload.cursor_token.clone_from(&previous.cursor_token);
    payload.server_epoch.clone_from(&previous.server_epoch);
    payload
}

fn changed_ids(payload: &SyncPayload) -> Vec<&str> {
    payload
        .changes
        .iter()
        .map(|row| row.todo_id.as_str())
        .collect()
}

#[tokio::test]
async fn incremental_pull_returns_only_changes_after_the_cursor() -> Result<()> {
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping anti-entropy test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    let tenant = "anti-entropy-tenant";
    sqlx_core::query::query("DELETE FROM todos WHERE tenant_id = $1")
        .bind(tenant)
        .execute(&pool)
        .await?;

    // First write establishes a cursor.
    let mut tx = pool.begin().await?;
    let first = merge_crdt_states(
        &mut tx,
        push("device-a", "row-1", "first", 1),
        "device-a",
        tenant,
        &todos_table(),
    )
    .await?;
    tx.commit().await?;
    let cursor = first.cursor;
    assert!(cursor > 0);
    assert!(first.cursor_token.is_some());
    assert!(first.server_epoch.is_some());

    // A cursor issued to another device cannot be used to skip this device's
    // first repair.
    let mut tx = pool.begin().await?;
    let reset = merge_crdt_states(
        &mut tx,
        pull("device-c", 1, cursor),
        "device-c",
        tenant,
        &todos_table(),
    )
    .await?;
    tx.commit().await?;
    assert!(reset.cursor_reset);
    assert!(changed_ids(&reset).contains(&"row-1"));

    // Even the correct device cannot forge or guess a cursor capability.
    let mut forged = continue_pull("device-a", 2, &first);
    forged.cursor_token = Some("forged-token".to_owned());
    let mut tx = pool.begin().await?;
    let reset = merge_crdt_states(&mut tx, forged, "device-a", tenant, &todos_table()).await?;
    tx.commit().await?;
    assert!(reset.cursor_reset);

    // Pulling at that cursor returns nothing new.
    let mut tx = pool.begin().await?;
    let idle = merge_crdt_states(
        &mut tx,
        continue_pull("device-a", 2, &first),
        "device-a",
        tenant,
        &todos_table(),
    )
    .await?;
    tx.commit().await?;
    assert!(idle.changes.is_empty());

    // A second device writes another row.
    let mut tx = pool.begin().await?;
    merge_crdt_states(
        &mut tx,
        push("device-b", "row-2", "second", 1),
        "device-b",
        tenant,
        &todos_table(),
    )
    .await?;
    tx.commit().await?;

    // Pulling at the OLD cursor returns only the new row, not the one already
    // below the cursor: an O(delta) change feed, not an O(tenant) scan.
    let mut tx = pool.begin().await?;
    let delta = merge_crdt_states(
        &mut tx,
        continue_pull("device-a", 5, &first),
        "device-a",
        tenant,
        &todos_table(),
    )
    .await?;
    tx.commit().await?;
    let ids = changed_ids(&delta);
    assert!(ids.contains(&"row-2"));
    assert!(!ids.contains(&"row-1"));
    assert!(delta.cursor > cursor);

    sqlx_core::query::query(
        "UPDATE loomabase_cursor_lease
         SET last_seen_at = clock_timestamp() - INTERVAL '2 days'
         WHERE tenant_id = $1 AND device_id = $2 AND crdt_table = $3",
    )
    .bind(tenant)
    .bind("device-a")
    .bind(todos_table().crdt_table())
    .execute(&pool)
    .await?;
    assert!(expire_cursor_leases(&pool, Duration::from_hours(24)).await? >= 1);

    let mut tx = pool.begin().await?;
    let repaired = merge_crdt_states(
        &mut tx,
        continue_pull("device-a", 6, &delta),
        "device-a",
        tenant,
        &todos_table(),
    )
    .await?;
    tx.commit().await?;
    assert!(repaired.cursor_reset);
    assert!(!repaired.changes.is_empty());
    Ok(())
}
