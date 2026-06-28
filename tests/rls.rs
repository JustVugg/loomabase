use std::collections::BTreeMap;

use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;
use loomabase::server::{initialize_server_schema, merge_crdt_states};
use sqlx_postgres::PgPool;

type BoxError = Box<dyn std::error::Error>;

fn title_payload(device_id: &str, title: &str) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.to_owned(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: "rls-shared".to_owned(),
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

/// Row-Level Security only constrains non-superusers, so this test seeds data as
/// the admin (superuser, which bypasses RLS) and then verifies isolation through
/// a dedicated limited role — exactly the production deployment shape.
#[tokio::test]
async fn rls_isolates_tenants_for_a_limited_role() -> Result<(), BoxError> {
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping RLS test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let admin = PgPool::connect(&database_url).await?;
    initialize_server_schema(&admin).await?;

    // A non-superuser application role with table access (RLS applies to it).
    sqlx_core::raw_sql::raw_sql(
        "DO $$ BEGIN
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'loomabase_rls_app') THEN
                CREATE ROLE loomabase_rls_app LOGIN PASSWORD 'rls_app';
            END IF;
        END $$;
        GRANT SELECT, INSERT, UPDATE, DELETE
            ON loomabase_state, loomabase_cursor_lease, todos, todos_crdt
            TO loomabase_rls_app;
        GRANT SELECT ON loomabase_server_state TO loomabase_rls_app;
        GRANT USAGE, SELECT ON SEQUENCE loomabase_seq TO loomabase_rls_app;",
    )
    .execute(&admin)
    .await?;

    // Seed two tenants' data for the same row id (as superuser, bypassing RLS).
    sqlx_core::query::query("DELETE FROM todos WHERE id = $1")
        .bind("rls-shared")
        .execute(&admin)
        .await?;
    for (tenant, device, title) in [
        ("rls-tenant-a", "device-a", "tenant A"),
        ("rls-tenant-b", "device-b", "tenant B"),
    ] {
        let mut tx = admin.begin().await?;
        merge_crdt_states(
            &mut tx,
            title_payload(device, title),
            device,
            tenant,
            &todos_table(),
        )
        .await?;
        tx.commit().await?;
    }

    // Connect as the limited role; RLS now governs every query.
    let app_url = database_url.replacen("postgres:postgres@", "loomabase_rls_app:rls_app@", 1);
    let app = PgPool::connect(&app_url).await?;

    // The exact runtime merge succeeds with only DML and sequence permissions.
    let mut tx = app.begin().await?;
    merge_crdt_states(
        &mut tx,
        title_payload("device-z", "limited role"),
        "device-z",
        "rls-tenant-a",
        &todos_table(),
    )
    .await?;
    tx.commit().await?;

    // With a tenant context, an UNFILTERED query sees only that tenant.
    let title_a = visible_title(&app, "rls-tenant-a").await?;
    assert_eq!(title_a.as_deref(), Some("limited role"));
    let title_b = visible_title(&app, "rls-tenant-b").await?;
    assert_eq!(title_b.as_deref(), Some("tenant B"));

    // A tenant cannot see another tenant's row even by its id.
    let mut tx = app.begin().await?;
    sqlx_core::query::query("SELECT set_config('loomabase.tenant_id', $1, true)")
        .bind("rls-tenant-a")
        .execute(&mut *tx)
        .await?;
    let visible_rows: i64 = sqlx_core::query_scalar::query_scalar("SELECT COUNT(*) FROM todos")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    assert_eq!(visible_rows, 1);

    // Without any context, the policy matches nothing (fail-safe).
    let mut tx = app.begin().await?;
    let unscoped: i64 =
        sqlx_core::query_scalar::query_scalar("SELECT COUNT(*) FROM todos WHERE id = $1")
            .bind("rls-shared")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    assert_eq!(unscoped, 0);
    Ok(())
}

async fn visible_title(pool: &PgPool, tenant: &str) -> Result<Option<String>, BoxError> {
    let mut tx = pool.begin().await?;
    sqlx_core::query::query("SELECT set_config('loomabase.tenant_id', $1, true)")
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    let title: Option<(String,)> =
        sqlx_core::query_as::query_as("SELECT title FROM todos WHERE id = $1")
            .bind("rls-shared")
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(title.map(|row| row.0))
}
