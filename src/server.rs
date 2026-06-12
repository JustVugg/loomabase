use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::TryStreamExt;
use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_core::types::Json;
use sqlx_postgres::{PgPool, Postgres};

use crate::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, MAX_PAYLOAD_CELLS, MAX_RESPONSE_BYTES,
    MAX_RESPONSE_CELLS, MergeDecision, RowChange, SERVER_DEVICE_ID, SyncPayload, decide_lww,
    validate_clock_advance, validate_identifier,
};
use crate::error::{Result, SyncError};
use crate::replica::{
    PartialReplicaRequest, PartialReplicaResponse, ReplicaPredicate, validate_member_ids,
};
use crate::schema::{ColumnType, TableDef, todos_table};

/// Transaction-scoped advisory lock key (ASCII "loomabas") that serializes
/// concurrent schema initialization. Concurrent `CREATE TABLE IF NOT EXISTS`
/// statements can otherwise race on the `PostgreSQL` system catalogs.
const SCHEMA_INIT_LOCK_KEY: i64 = 0x6C6F_6F6D_6162_6173;

/// Initializes the canonical `todos` schema with an atomic commit.
pub async fn initialize_server_schema(pool: &PgPool) -> Result<()> {
    initialize_server_schema_with(pool, &todos_table()).await
}

/// Initializes an arbitrary contract's schema with an atomic commit. Safe to
/// call concurrently from many processes: the advisory lock serializes catalog
/// mutations.
pub async fn initialize_server_schema_with(pool: &PgPool, table: &TableDef) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx_core::query::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SCHEMA_INIT_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    sqlx_core::query::query("CREATE SEQUENCE IF NOT EXISTS loomabase_seq")
        .execute(&mut *tx)
        .await?;

    // Migrate an existing table up to the contract before (re)creating the
    // schema. On a fresh database there are no columns and this is a no-op.
    let existing = postgres_existing_columns(&mut tx, table.name()).await?;
    if !existing.is_empty() {
        for statement in table.postgres_migration_sql(&existing)? {
            sqlx_core::raw_sql::raw_sql(&statement)
                .execute(&mut *tx)
                .await?;
        }
    }
    let existing_crdt = postgres_existing_columns(&mut tx, &table.crdt_table()).await?;
    for statement in table.postgres_crdt_migration_sql(&existing_crdt)? {
        sqlx_core::raw_sql::raw_sql(&statement)
            .execute(&mut *tx)
            .await?;
    }
    let schema = table.postgres_schema();
    sqlx_core::raw_sql::raw_sql(&schema)
        .execute(&mut *tx)
        .await?;
    sqlx_core::raw_sql::raw_sql(
        "ALTER TABLE loomabase_cursor_lease
            ADD COLUMN IF NOT EXISTS cursor_token TEXT;
         ALTER TABLE loomabase_cursor_lease
            ADD COLUMN IF NOT EXISTS server_epoch TEXT;
         ALTER TABLE loomabase_cursor_lease
            ADD COLUMN IF NOT EXISTS last_seen_at TIMESTAMPTZ;
         UPDATE loomabase_cursor_lease
            SET cursor_token = COALESCE(cursor_token, gen_random_uuid()::text),
                server_epoch = COALESCE(
                    server_epoch,
                    (SELECT server_epoch FROM loomabase_server_state WHERE singleton)
                ),
                last_seen_at = COALESCE(last_seen_at, clock_timestamp());
         ALTER TABLE loomabase_cursor_lease ALTER COLUMN cursor_token SET NOT NULL;
         ALTER TABLE loomabase_cursor_lease ALTER COLUMN server_epoch SET NOT NULL;
         ALTER TABLE loomabase_cursor_lease ALTER COLUMN last_seen_at SET NOT NULL;
         ALTER TABLE loomabase_cursor_lease
            ALTER COLUMN cursor_token SET DEFAULT gen_random_uuid()::text;
         ALTER TABLE loomabase_cursor_lease
            ALTER COLUMN last_seen_at SET DEFAULT clock_timestamp();",
    )
    .execute(&mut *tx)
    .await?;

    // Recreate the exact policy rather than trusting a policy with the expected
    // name: a same-named permissive policy would otherwise silently weaken RLS.
    sqlx_core::raw_sql::raw_sql(&table.postgres_rls_policies())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Reads the existing column names and data types of a table, or an empty map
/// when the table does not yet exist.
async fn postgres_existing_columns(
    tx: &mut Transaction<'_, Postgres>,
    table_name: &str,
) -> Result<BTreeMap<String, String>> {
    let rows = sqlx_core::query::query(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_name = $1 AND table_schema = current_schema()",
    )
    .bind(table_name)
    .fetch_all(&mut **tx)
    .await?;
    let mut columns = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get("column_name")?;
        let data_type: String = row.try_get("data_type")?;
        columns.insert(name, data_type);
    }
    Ok(columns)
}

/// Transactional, tenant-scoped `PostgreSQL` merge. The function is async because
/// it performs I/O. `tenant_id` is established by the authenticated caller, never
/// taken from the untrusted payload; every row and the clock are isolated to it.
pub async fn merge_crdt_states(
    server_tx: &mut Transaction<'_, Postgres>,
    client_payload: SyncPayload,
    client_device_id: &str,
    tenant_id: &str,
    table: &TableDef,
) -> Result<SyncPayload> {
    validate_identifier("tenant_id", tenant_id)?;
    client_payload.validate_client_request(client_device_id, table)?;
    let observed_clock = client_payload.max_observed_clock();
    let app_table = table.name();
    let crdt_table = table.crdt_table();
    let response_protocol_version = client_payload.protocol_version;

    // Migrations hold the exclusive form of this lock. Normal merges take a
    // shared lock, allowing concurrent sync while preventing DDL/DML lock-order
    // deadlocks during a rolling process start.
    sqlx_core::query::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(SCHEMA_INIT_LOCK_KEY)
        .execute(&mut **server_tx)
        .await?;

    // Scope the whole transaction to this tenant for Row-Level Security, so even
    // a query that omits its tenant filter cannot read or write another tenant.
    sqlx_core::query::query("SELECT set_config('loomabase.tenant_id', $1, true)")
        .bind(tenant_id)
        .execute(&mut **server_tx)
        .await?;

    // Lazily create and lock only this tenant's clock row, so concurrent tenants
    // never serialize against each other.
    let server_clock: i64 = sqlx_core::query_scalar::query_scalar(
        "INSERT INTO loomabase_state(tenant_id, lamport_clock) VALUES ($1, 0)
         ON CONFLICT(tenant_id) DO UPDATE SET lamport_clock = loomabase_state.lamport_clock
         RETURNING lamport_clock",
    )
    .bind(tenant_id)
    .fetch_one(&mut **server_tx)
    .await?;
    let server_clock = clock_from_i64(server_clock)?;
    validate_clock_advance(server_clock, observed_clock)?;

    for row in &client_payload.changes {
        sqlx_core::query::query(&format!(
            "INSERT INTO {app_table}(tenant_id, id) VALUES ($1, $2)
             ON CONFLICT(tenant_id, id) DO NOTHING"
        ))
        .bind(tenant_id)
        .bind(&row.todo_id)
        .execute(&mut **server_tx)
        .await?;

        for (column_name, incoming) in &row.columns {
            let current: Option<(Json<CrdtValue>, i64, String)> =
                sqlx_core::query_as::query_as(&format!(
                    "SELECT value, lamport_clock, device_id
                 FROM {crdt_table}
                 WHERE tenant_id = $1 AND todo_id = $2 AND column_name = $3
                 FOR UPDATE"
                ))
                .bind(tenant_id)
                .bind(&row.todo_id)
                .bind(column_name)
                .fetch_optional(&mut **server_tx)
                .await?;

            let should_apply = match current {
                None => true,
                Some((Json(current_value), clock, device_id)) => {
                    let current_metadata = ColumnMetadata {
                        lamport_clock: clock_from_i64(clock)?,
                        device_id,
                    };
                    match decide_lww(&current_metadata, &incoming.metadata) {
                        MergeDecision::AcceptIncoming => true,
                        MergeDecision::KeepCurrent => false,
                        MergeDecision::Equal if current_value == incoming.value => false,
                        MergeDecision::Equal => {
                            return Err(SyncError::InvalidPayload(
                                "the same CRDT version cannot identify different values".to_owned(),
                            ));
                        }
                    }
                }
            };

            if should_apply {
                write_server_todo_column(
                    server_tx,
                    table,
                    tenant_id,
                    &row.todo_id,
                    column_name,
                    &incoming.value,
                )
                .await?;
                sqlx_core::query::query(&format!(
                    "INSERT INTO {crdt_table}
                        (tenant_id, todo_id, column_name, value, lamport_clock, device_id, seq)
                     VALUES ($1, $2, $3, $4, $5, $6, nextval('loomabase_seq'))
                     ON CONFLICT(tenant_id, todo_id, column_name) DO UPDATE SET
                        value = EXCLUDED.value,
                        lamport_clock = EXCLUDED.lamport_clock,
                        device_id = EXCLUDED.device_id,
                        seq = EXCLUDED.seq"
                ))
                .bind(tenant_id)
                .bind(&row.todo_id)
                .bind(column_name)
                .bind(Json(&incoming.value))
                .bind(clock_to_i64(incoming.metadata.lamport_clock)?)
                .bind(&incoming.metadata.device_id)
                .execute(&mut **server_tx)
                .await?;
            }
        }
    }

    let next_server_clock = server_clock
        .max(observed_clock)
        .checked_add(1)
        .ok_or(SyncError::ClockOverflow)?;
    sqlx_core::query::query("UPDATE loomabase_state SET lamport_clock = $1 WHERE tenant_id = $2")
        .bind(clock_to_i64(next_server_clock)?)
        .bind(tenant_id)
        .execute(&mut **server_tx)
        .await?;

    let tenant_max_cursor: i64 = sqlx_core::query_scalar::query_scalar(&format!(
        "SELECT COALESCE(MAX(seq), 0) FROM {crdt_table} WHERE tenant_id = $1"
    ))
    .bind(tenant_id)
    .fetch_one(&mut **server_tx)
    .await?;
    let server_epoch: String = sqlx_core::query_scalar::query_scalar(
        "SELECT server_epoch FROM loomabase_server_state WHERE singleton",
    )
    .fetch_one(&mut **server_tx)
    .await?;
    let lease: Option<(i64, String, String)> = sqlx_core::query_as::query_as(
        "SELECT max_issued_cursor, cursor_token, server_epoch
         FROM loomabase_cursor_lease
         WHERE tenant_id = $1 AND device_id = $2 AND crdt_table = $3
         FOR UPDATE",
    )
    .bind(tenant_id)
    .bind(client_device_id)
    .bind(&crdt_table)
    .fetch_optional(&mut **server_tx)
    .await?;
    let cursor_valid = client_payload.cursor == 0
        || lease
            .as_ref()
            .is_some_and(|(max_issued_cursor, cursor_token, lease_epoch)| {
                client_payload.cursor <= tenant_max_cursor
                    && client_payload.cursor <= *max_issued_cursor
                    && (response_protocol_version < 4
                        || (client_payload.cursor_token.as_deref() == Some(cursor_token)
                            && client_payload.server_epoch.as_deref() == Some(lease_epoch)
                            && lease_epoch == &server_epoch))
            });
    let cursor_reset = client_payload.cursor != 0 && !cursor_valid;
    let effective_cursor = if cursor_valid {
        client_payload.cursor
    } else {
        0
    };

    // Incremental bounded change feed. Fetching one extra cell determines
    // whether another page remains without advancing beyond applied data.
    let query_limit = i64::try_from(MAX_RESPONSE_CELLS + 1)
        .map_err(|_| SyncError::InvalidPayload("response limit is not storable".to_owned()))?;
    let query = format!(
        "SELECT todo_id, column_name, value, lamport_clock, device_id, seq
         FROM {crdt_table} WHERE tenant_id = $1 AND seq > $2 ORDER BY seq LIMIT $3"
    );
    let mut response_by_row: BTreeMap<String, BTreeMap<String, CrdtColumn>> = BTreeMap::new();
    let mut response_bytes = 0_usize;
    let mut included = 0_usize;
    let mut next_cursor = effective_cursor;
    let mut has_more = false;
    let mut rows = sqlx_core::query::query(&query)
        .bind(tenant_id)
        .bind(effective_cursor)
        .bind(query_limit)
        .fetch(&mut **server_tx);
    while let Some(row) = rows.try_next().await? {
        let todo_id: String = row.try_get("todo_id")?;
        let column_name: String = row.try_get("column_name")?;
        let Json(value): Json<CrdtValue> = row.try_get("value")?;
        let metadata = ColumnMetadata {
            lamport_clock: clock_from_i64(row.try_get("lamport_clock")?)?,
            device_id: row.try_get("device_id")?,
        };
        let cell_bytes = serde_json::to_vec(&value)?.len()
            + todo_id.len()
            + column_name.len()
            + metadata.device_id.len();
        if included >= MAX_RESPONSE_CELLS
            || (included > 0 && response_bytes + cell_bytes > MAX_RESPONSE_BYTES)
        {
            has_more = true;
            break;
        }
        response_bytes += cell_bytes;
        included += 1;
        next_cursor = row.try_get("seq")?;
        response_by_row
            .entry(todo_id)
            .or_default()
            .insert(column_name, CrdtColumn { value, metadata });
    }
    drop(rows);
    if !has_more {
        next_cursor = tenant_max_cursor;
    }
    let cursor_token: String = sqlx_core::query_scalar::query_scalar(
        "INSERT INTO loomabase_cursor_lease
            (tenant_id, device_id, crdt_table, max_issued_cursor, server_epoch)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT(tenant_id, device_id, crdt_table) DO UPDATE SET
            max_issued_cursor = GREATEST(
                loomabase_cursor_lease.max_issued_cursor,
                EXCLUDED.max_issued_cursor
            ),
            cursor_token = CASE
                WHEN loomabase_cursor_lease.server_epoch = EXCLUDED.server_epoch
                    THEN loomabase_cursor_lease.cursor_token
                ELSE gen_random_uuid()::text
            END,
            server_epoch = EXCLUDED.server_epoch,
            last_seen_at = clock_timestamp()
         RETURNING cursor_token",
    )
    .bind(tenant_id)
    .bind(client_device_id)
    .bind(&crdt_table)
    .bind(next_cursor)
    .bind(&server_epoch)
    .fetch_one(&mut **server_tx)
    .await?;

    Ok(SyncPayload {
        protocol_version: response_protocol_version,
        schema_fingerprint: table.fingerprint(),
        source_device_id: SERVER_DEVICE_ID.to_owned(),
        source_lamport: next_server_clock,
        changes: response_by_row
            .into_iter()
            .map(|(todo_id, columns)| RowChange { todo_id, columns })
            .collect(),
        cursor: next_cursor,
        has_more,
        cursor_reset,
        cursor_token: Some(cursor_token),
        server_epoch: Some(server_epoch),
    })
}

/// Merges client writes and returns a complete authoritative snapshot for one
/// partial-replica scope. Membership is recomputed after the merge, so writes
/// that move rows into or out of the scope are reflected in the same
/// transaction. Oversized scopes fail explicitly instead of being truncated.
pub async fn merge_partial_replica(
    server_tx: &mut Transaction<'_, Postgres>,
    request: PartialReplicaRequest,
    client_device_id: &str,
    tenant_id: &str,
    table: &TableDef,
) -> Result<PartialReplicaResponse> {
    request.validate(table, client_device_id)?;
    let scope_id = request.scope_id.clone();
    let scope_version = request.scope_version;
    let interest = request.interest.clone();
    let known_member_ids = request.known_member_ids.clone();
    let mut sync =
        merge_crdt_states(server_tx, request.sync, client_device_id, tenant_id, table).await?;

    let mut membership_query = QueryBuilder::<Postgres>::new(format!(
        "SELECT id FROM {} WHERE tenant_id = ",
        table.name()
    ));
    membership_query
        .push_bind(tenant_id)
        .push(" AND deleted = FALSE");
    for predicate in &interest.predicates {
        match predicate {
            ReplicaPredicate::IdEquals(value) => {
                membership_query.push(" AND id = ").push_bind(value);
            }
            ReplicaPredicate::IdPrefix(prefix) => {
                membership_query
                    .push(" AND starts_with(id, ")
                    .push_bind(prefix)
                    .push(')');
            }
            ReplicaPredicate::ColumnEquals { column, value } => {
                membership_query.push(format!(" AND {column} = "));
                match value {
                    CrdtValue::Integer(value) => {
                        membership_query.push_bind(*value);
                    }
                    CrdtValue::Real(value) => {
                        membership_query.push_bind(*value);
                    }
                    CrdtValue::Text(value) => {
                        membership_query.push_bind(value);
                    }
                    CrdtValue::Boolean(value) => {
                        membership_query.push_bind(*value);
                    }
                    CrdtValue::Null | CrdtValue::Blob(_) => {
                        return Err(SyncError::InvalidPayload(format!(
                            "invalid partial-replica predicate value for {column}"
                        )));
                    }
                }
            }
        }
    }
    membership_query.push(format!(
        " ORDER BY id LIMIT {}",
        u64::from(interest.limit) + 1
    ));
    let mut member_ids: Vec<String> = membership_query
        .build_query_scalar()
        .fetch_all(&mut **server_tx)
        .await?;
    if member_ids.len() > interest.limit as usize {
        return Err(SyncError::InvalidPayload(format!(
            "partial replica scope exceeds its declared limit of {} rows; narrow the interest or increase the limit",
            interest.limit
        )));
    }
    validate_member_ids(&member_ids)?;

    let crdt_table = table.crdt_table();
    let mut snapshot_by_row: BTreeMap<String, BTreeMap<String, CrdtColumn>> = BTreeMap::new();
    let mut snapshot_bytes = 0_usize;
    let mut snapshot_cells = 0_usize;
    if !member_ids.is_empty() {
        let snapshot_query = format!(
            "SELECT todo_id, column_name, value, lamport_clock, device_id
             FROM {crdt_table}
             WHERE tenant_id = $1 AND todo_id = ANY($2::text[])
             ORDER BY todo_id, column_name"
        );
        let rows = sqlx_core::query::query(&snapshot_query)
            .bind(tenant_id)
            .bind(&member_ids)
            .fetch_all(&mut **server_tx)
            .await?;
        for row in rows {
            snapshot_cells += 1;
            if snapshot_cells > MAX_PAYLOAD_CELLS {
                return Err(SyncError::InvalidPayload(format!(
                    "partial replica snapshot exceeds the {MAX_PAYLOAD_CELLS} cell limit"
                )));
            }
            let todo_id: String = row.try_get("todo_id")?;
            let column_name: String = row.try_get("column_name")?;
            let Json(value): Json<CrdtValue> = row.try_get("value")?;
            let metadata = ColumnMetadata {
                lamport_clock: clock_from_i64(row.try_get("lamport_clock")?)?,
                device_id: row.try_get("device_id")?,
            };
            snapshot_bytes += serde_json::to_vec(&value)?.len()
                + todo_id.len()
                + column_name.len()
                + metadata.device_id.len();
            if snapshot_bytes > MAX_RESPONSE_BYTES {
                return Err(SyncError::InvalidPayload(format!(
                    "partial replica snapshot exceeds the {MAX_RESPONSE_BYTES}-byte limit"
                )));
            }
            snapshot_by_row
                .entry(todo_id)
                .or_default()
                .insert(column_name, CrdtColumn { value, metadata });
        }
    }

    let evicted_row_ids = known_member_ids
        .iter()
        .filter(|row_id| member_ids.binary_search(row_id).is_err())
        .cloned()
        .collect::<Vec<_>>();
    sync.changes = snapshot_by_row
        .into_iter()
        .map(|(todo_id, columns)| RowChange { todo_id, columns })
        .collect();
    sync.has_more = false;
    let response = PartialReplicaResponse {
        scope_id,
        scope_version,
        member_ids: std::mem::take(&mut member_ids),
        evicted_row_ids,
        sync,
    };
    response.validate(table)?;
    Ok(response)
}

/// Expires inactive cursor capabilities. A returning device automatically
/// receives `cursor_reset` and performs a bounded full repair.
pub async fn expire_cursor_leases(pool: &PgPool, inactive_for: Duration) -> Result<u64> {
    let seconds = i64::try_from(inactive_for.as_secs())
        .map_err(|_| SyncError::InvalidPayload("lease TTL is too large".to_owned()))?;
    let result = sqlx_core::query::query(
        "DELETE FROM loomabase_cursor_lease
         WHERE last_seen_at < clock_timestamp() - make_interval(secs => $1)",
    )
    .bind(seconds)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Rotates the server data epoch and invalidates every issued cursor. Run this
/// after restoring a database into a divergent history.
pub async fn rotate_server_epoch(pool: &PgPool) -> Result<String> {
    let mut tx = pool.begin().await?;
    let epoch: String = sqlx_core::query_scalar::query_scalar(
        "UPDATE loomabase_server_state
         SET server_epoch = gen_random_uuid()::text
         WHERE singleton
         RETURNING server_epoch",
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx_core::query::query("DELETE FROM loomabase_cursor_lease")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(epoch)
}

async fn write_server_todo_column(
    tx: &mut Transaction<'_, Postgres>,
    table: &TableDef,
    tenant_id: &str,
    todo_id: &str,
    column_name: &str,
    value: &CrdtValue,
) -> Result<()> {
    let Some(ty) = table.column_type(column_name) else {
        return Err(SyncError::InvalidPayload(format!(
            "column is not synchronizable: {column_name}"
        )));
    };
    let sql = format!(
        "UPDATE {} SET {column_name} = $3 WHERE tenant_id = $1 AND id = $2",
        table.name()
    );
    let query = sqlx_core::query::query(&sql).bind(tenant_id).bind(todo_id);
    match (ty, value) {
        (ColumnType::Text, CrdtValue::Text(text)) => {
            query.bind(text).execute(&mut **tx).await?;
        }
        (ColumnType::Integer, CrdtValue::Integer(integer)) => {
            query.bind(integer).execute(&mut **tx).await?;
        }
        (ColumnType::Real, CrdtValue::Real(real)) => {
            query.bind(real).execute(&mut **tx).await?;
        }
        (ColumnType::Boolean, CrdtValue::Boolean(boolean)) => {
            query.bind(boolean).execute(&mut **tx).await?;
        }
        _ => {
            return Err(SyncError::InvalidPayload(format!(
                "value type is incompatible with column {column_name}"
            )));
        }
    }
    Ok(())
}

fn clock_from_i64(clock: i64) -> Result<u64> {
    u64::try_from(clock)
        .map_err(|_| SyncError::InvalidPayload("negative clock stored in database".to_owned()))
}

fn clock_to_i64(clock: u64) -> Result<i64> {
    i64::try_from(clock).map_err(|_| SyncError::ClockOverflow)
}
