use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, DELETED_COLUMN, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::policy::{ColumnAllowListAuthorizer, NoopValidator, SyncSecurity};
use loomabase::replica::{PartialReplicaRequest, ReplicaInterest, ReplicaPredicate};
use loomabase::schema::{ColumnDef, ColumnType, TableDef, todos_table};
use loomabase::server::{
    initialize_server_schema, initialize_server_schema_with, merge_crdt_states,
    merge_crdt_states_with_security, merge_partial_replica,
};
use sqlx_postgres::PgPool;
use std::sync::Arc;

static POSTGRES_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn payload(device_id: &str, column_name: &str, value: CrdtValue) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.to_owned(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: "postgres-integration-todo".to_owned(),
            columns: BTreeMap::from([(
                column_name.to_owned(),
                CrdtColumn {
                    value,
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
async fn postgres_adapter_merges_columns_and_is_idempotent() -> Result<()> {
    let _guard = POSTGRES_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping PostgreSQL integration test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    sqlx_core::query::query("DELETE FROM todos WHERE id = $1")
        .bind("postgres-integration-todo")
        .execute(&pool)
        .await?;

    let title = payload(
        "device-a",
        "title",
        CrdtValue::Text("from device A".to_owned()),
    );
    let completed = payload("device-b", "completed", CrdtValue::Boolean(true));

    let mut tx = pool.begin().await?;
    merge_crdt_states(
        &mut tx,
        title.clone(),
        "device-a",
        "pg-tenant",
        &todos_table(),
    )
    .await?;
    tx.commit().await?;

    let mut tx = pool.begin().await?;
    merge_crdt_states(&mut tx, completed, "device-b", "pg-tenant", &todos_table()).await?;
    tx.commit().await?;

    let mut tx = pool.begin().await?;
    merge_crdt_states(&mut tx, title, "device-a", "pg-tenant", &todos_table()).await?;
    tx.commit().await?;

    let row: (String, bool) =
        sqlx_core::query_as::query_as("SELECT title, completed FROM todos WHERE id = $1")
            .bind("postgres-integration-todo")
            .fetch_one(&pool)
            .await?;
    assert_eq!(row, ("from device A".to_owned(), true));
    Ok(())
}

#[tokio::test]
async fn postgres_security_hooks_reject_and_audit_cells() -> Result<()> {
    let _guard = POSTGRES_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping PostgreSQL integration test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    let table = todos_table();
    initialize_server_schema(&pool).await?;
    let tenant = "pg-security-tenant";
    let row_id = "pg-security-todo";
    sqlx_core::query::query("DELETE FROM todos WHERE tenant_id = $1 AND id = $2")
        .bind(tenant)
        .bind(row_id)
        .execute(&pool)
        .await?;
    sqlx_core::query::query(
        "DELETE FROM loomabase_audit_log WHERE tenant_id = $1 AND todo_id = $2",
    )
    .bind(tenant)
    .bind(row_id)
    .execute(&pool)
    .await?;

    let mut denied = payload(
        "device-a",
        "title",
        CrdtValue::Text("must not land".to_owned()),
    );
    denied.changes[0].todo_id = row_id.to_owned();
    let security = SyncSecurity::new(
        Arc::new(ColumnAllowListAuthorizer::new(&table, ["completed"])?),
        Arc::new(NoopValidator),
        loomabase::policy::AuditMode::Database,
    );

    let mut tx = pool.begin().await?;
    let response =
        merge_crdt_states_with_security(&mut tx, denied, "device-a", tenant, &table, &security)
            .await?;
    tx.commit().await?;

    assert_eq!(response.rejections.len(), 1);
    assert_eq!(response.rejections[0].column_name, "title");
    let title: String = sqlx_core::query_scalar::query_scalar(
        "SELECT title FROM todos WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(row_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(title, "");
    let outcome: String = sqlx_core::query_scalar::query_scalar(
        "SELECT outcome FROM loomabase_audit_log
         WHERE tenant_id = $1 AND todo_id = $2 AND column_name = 'title'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(row_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(outcome, "rejected_authorization");
    Ok(())
}

#[tokio::test]
async fn postgres_adapter_applies_tombstones() -> Result<()> {
    let _guard = POSTGRES_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping PostgreSQL integration test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    sqlx_core::query::query("DELETE FROM todos WHERE id = $1")
        .bind("postgres-tombstone-todo")
        .execute(&pool)
        .await?;

    let create = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "device-a".to_owned(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: "postgres-tombstone-todo".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text("to be deleted".to_owned()),
                    metadata: ColumnMetadata {
                        lamport_clock: 1,
                        device_id: "device-a".to_owned(),
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
    let delete = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "device-a".to_owned(),
        source_lamport: 5,
        changes: vec![RowChange {
            todo_id: "postgres-tombstone-todo".to_owned(),
            columns: BTreeMap::from([(
                DELETED_COLUMN.to_owned(),
                CrdtColumn {
                    value: CrdtValue::Boolean(true),
                    metadata: ColumnMetadata {
                        lamport_clock: 5,
                        device_id: "device-a".to_owned(),
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

    let mut tx = pool.begin().await?;
    merge_crdt_states(&mut tx, create, "device-a", "pg-tenant", &todos_table()).await?;
    tx.commit().await?;

    let mut tx = pool.begin().await?;
    merge_crdt_states(&mut tx, delete, "device-a", "pg-tenant", &todos_table()).await?;
    tx.commit().await?;

    let deleted: bool =
        sqlx_core::query_scalar::query_scalar("SELECT deleted FROM todos WHERE id = $1")
            .bind("postgres-tombstone-todo")
            .fetch_one(&pool)
            .await?;
    assert!(deleted);
    Ok(())
}

#[tokio::test]
async fn postgres_partial_replica_returns_authoritative_membership_and_evictions() -> Result<()> {
    let _guard = POSTGRES_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping PostgreSQL integration test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    let table = todos_table();
    initialize_server_schema(&pool).await?;
    let tenant = "partial-pg-tenant";
    let row_id = "partial-pg-todo";
    sqlx_core::query::query("DELETE FROM todos WHERE tenant_id = $1 AND id = $2")
        .bind(tenant)
        .bind(row_id)
        .execute(&pool)
        .await?;

    let mut seed = payload(
        "partial-writer",
        "title",
        CrdtValue::Text("authoritative".to_owned()),
    );
    seed.changes[0].todo_id = row_id.to_owned();
    let mut tx = pool.begin().await?;
    merge_crdt_states(&mut tx, seed, "partial-writer", tenant, &table).await?;
    tx.commit().await?;

    let interest = ReplicaInterest {
        predicates: vec![ReplicaPredicate::ColumnEquals {
            column: "completed".to_owned(),
            value: CrdtValue::Boolean(false),
        }],
        limit: 100,
    };
    let first = PartialReplicaRequest {
        scope_id: "incomplete".to_owned(),
        scope_version: 1,
        interest: interest.clone(),
        known_member_ids: Vec::new(),
        sync: SyncPayload::empty("partial-client", 0, &table),
    };
    let mut tx = pool.begin().await?;
    let first_response =
        merge_partial_replica(&mut tx, first, "partial-client", tenant, &table).await?;
    tx.commit().await?;
    assert_eq!(first_response.member_ids, [row_id]);
    assert!(first_response.evicted_row_ids.is_empty());
    assert_eq!(first_response.sync.changes.len(), 1);

    let mut completed = payload("partial-writer", "completed", CrdtValue::Boolean(true));
    completed.changes[0].todo_id = row_id.to_owned();
    completed.source_lamport = 2;
    completed.changes[0]
        .columns
        .get_mut("completed")
        .unwrap()
        .metadata
        .lamport_clock = 2;
    let mut tx = pool.begin().await?;
    merge_crdt_states(&mut tx, completed, "partial-writer", tenant, &table).await?;
    tx.commit().await?;

    let second = PartialReplicaRequest {
        scope_id: "incomplete".to_owned(),
        scope_version: 2,
        interest,
        known_member_ids: vec![row_id.to_owned()],
        sync: SyncPayload::empty("partial-client", 0, &table),
    };
    let mut tx = pool.begin().await?;
    let second_response =
        merge_partial_replica(&mut tx, second, "partial-client", tenant, &table).await?;
    tx.commit().await?;
    assert!(second_response.member_ids.is_empty());
    assert_eq!(second_response.evicted_row_ids, [row_id]);
    assert!(second_response.sync.changes.is_empty());
    Ok(())
}

#[tokio::test]
async fn server_schema_upgrade_backfills_missing_change_feed_sequences() -> Result<()> {
    let _guard = POSTGRES_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping PostgreSQL integration test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    let table = TableDef::new(
        "upgrade_probe",
        vec![ColumnDef::new("body", ColumnType::Text)],
    )?;
    initialize_server_schema_with(&pool, &table).await?;
    sqlx_core::query::query("DELETE FROM upgrade_probe WHERE tenant_id = $1")
        .bind("upgrade-tenant")
        .execute(&pool)
        .await?;
    sqlx_core::raw_sql::raw_sql(
        "ALTER TABLE upgrade_probe_crdt DROP COLUMN IF EXISTS seq CASCADE;
         INSERT INTO upgrade_probe(tenant_id, id, body)
             VALUES ('upgrade-tenant', 'legacy-row', 'legacy');
         INSERT INTO upgrade_probe_crdt
             (tenant_id, todo_id, column_name, value, lamport_clock, device_id)
             VALUES (
                 'upgrade-tenant',
                 'legacy-row',
                 'body',
                 '{\"type\":\"text\",\"value\":\"legacy\"}'::jsonb,
                 1,
                 'legacy-device'
             );",
    )
    .execute(&pool)
    .await?;

    initialize_server_schema_with(&pool, &table).await?;

    let seq: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT seq FROM upgrade_probe_crdt
         WHERE tenant_id = 'upgrade-tenant' AND todo_id = 'legacy-row'",
    )
    .fetch_one(&pool)
    .await?;
    let default: Option<String> = sqlx_core::query_scalar::query_scalar(
        "SELECT column_default FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'upgrade_probe_crdt'
           AND column_name = 'seq'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(seq > 0);
    assert!(default.is_some_and(|value| value.contains("nextval")));
    Ok(())
}
