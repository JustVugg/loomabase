use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;
use loomabase::server::{initialize_server_schema, merge_crdt_states};
use sqlx_postgres::PgPool;

fn title_payload(device_id: &str, title: &str) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.to_owned(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: "shared-id".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text(title.to_owned()),
                    metadata: ColumnMetadata {
                        lamport_clock: 1,
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
async fn tenants_are_isolated_with_independent_clocks() -> Result<()> {
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping multi-tenancy test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    sqlx_core::query::query("DELETE FROM todos WHERE id = $1")
        .bind("shared-id")
        .execute(&pool)
        .await?;

    // Two tenants write the SAME row id with different values.
    let mut tx = pool.begin().await?;
    merge_crdt_states(
        &mut tx,
        title_payload("device-a", "tenant A note"),
        "device-a",
        "tenant-a",
        &todos_table(),
    )
    .await?;
    tx.commit().await?;

    let mut tx = pool.begin().await?;
    merge_crdt_states(
        &mut tx,
        title_payload("device-b", "tenant B note"),
        "device-b",
        "tenant-b",
        &todos_table(),
    )
    .await?;
    tx.commit().await?;

    // Each tenant keeps its own value for the shared id.
    let tenant_a: (String,) =
        sqlx_core::query_as::query_as("SELECT title FROM todos WHERE tenant_id = $1 AND id = $2")
            .bind("tenant-a")
            .bind("shared-id")
            .fetch_one(&pool)
            .await?;
    let tenant_b: (String,) =
        sqlx_core::query_as::query_as("SELECT title FROM todos WHERE tenant_id = $1 AND id = $2")
            .bind("tenant-b")
            .bind("shared-id")
            .fetch_one(&pool)
            .await?;
    assert_eq!(tenant_a.0, "tenant A note");
    assert_eq!(tenant_b.0, "tenant B note");

    // Each tenant has its own clock row.
    let clock_rows: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT COUNT(*) FROM loomabase_state WHERE tenant_id IN ($1, $2)",
    )
    .bind("tenant-a")
    .bind("tenant-b")
    .fetch_one(&pool)
    .await?;
    assert_eq!(clock_rows, 2);

    // A full pull for tenant A surfaces only tenant A's value, never tenant B's.
    let mut tx = pool.begin().await?;
    let response = merge_crdt_states(
        &mut tx,
        SyncPayload::empty("device-a", 5, &todos_table()),
        "device-a",
        "tenant-a",
        &todos_table(),
    )
    .await?;
    tx.commit().await?;

    let title = response
        .changes
        .iter()
        .find(|row| row.todo_id == "shared-id")
        .and_then(|row| row.columns.get("title"))
        .map(|column| column.value.clone());
    assert_eq!(title, Some(CrdtValue::Text("tenant A note".to_owned())));
    Ok(())
}
