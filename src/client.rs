use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::types::Value as SqlValue;
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter,
};
use serde::{Deserialize, Serialize};

use crate::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, MergeDecision, PROTOCOL_VERSION, RowChange, SyncPayload,
    TITLE_COLUMN, decide_lww, validate_column, validate_identifier,
};
use crate::error::{Result, SyncError};
use crate::replica::{PartialReplicaRequest, PartialReplicaResponse, ReplicaInterest};
use crate::schema::{
    ColumnType, Contract, LIVENESS_COLUMN, PRIMARY_KEY_COLUMN, TableDef, todos_table,
};

const MAX_SYNC_PAGES_PER_CALL: usize = 100;

/// Materialized application row exposed by the typed `todos` convenience facade.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub title: String,
    pub completed: bool,
}

/// `Send + Sync` facade: all rusqlite I/O runs in Tokio's blocking pool. Each
/// client is bound to one synchronization [`TableDef`] contract.
#[derive(Clone)]
pub struct SqliteClient {
    connection: Arc<Mutex<Connection>>,
    table: Arc<TableDef>,
}

impl fmt::Debug for SqliteClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SqliteClient")
            .field("table", &self.table.name())
            .finish_non_exhaustive()
    }
}

impl SqliteClient {
    /// Opens a client for the canonical `todos` contract.
    pub async fn open(path: impl AsRef<Path>, device_id: impl Into<String>) -> Result<Self> {
        Self::open_with(path, device_id, todos_table()).await
    }

    /// Opens a client bound to an arbitrary synchronization contract.
    pub async fn open_with(
        path: impl AsRef<Path>,
        device_id: impl Into<String>,
        table: TableDef,
    ) -> Result<Self> {
        let path: PathBuf = path.as_ref().to_owned();
        let device_id = device_id.into();
        validate_identifier("device_id", &device_id)?;
        let schema_table = table.clone();
        let connection = tokio::task::spawn_blocking(move || {
            let mut connection = Connection::open(path)?;
            initialize_client_with(&mut connection, &device_id, &schema_table)?;
            Ok::<_, SyncError>(connection)
        })
        .await
        .map_err(|error| SyncError::BlockingTask(error.to_string()))??;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            table: Arc::new(table),
        })
    }

    /// The contract this client synchronizes.
    #[must_use]
    pub fn table(&self) -> &TableDef {
        &self.table
    }

    // --- Generic row API -------------------------------------------------

    /// Inserts a new row, capturing the provided columns and the defaults of
    /// any column left unset. Unprovided columns take their schema default.
    pub async fn insert(&self, id: String, values: BTreeMap<String, CrdtValue>) -> Result<()> {
        validate_identifier("row_id", &id)?;
        for (column, value) in &values {
            reject_liveness_column(column)?;
            validate_column(&self.table, column, value)?;
        }
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut columns = vec![PRIMARY_KEY_COLUMN.to_owned()];
            let mut bind: Vec<SqlValue> = vec![SqlValue::Text(id)];
            for (column, value) in values {
                columns.push(column);
                bind.push(crdt_to_sqlite(&value)?);
            }
            let placeholders = (1..=columns.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO {}({}) VALUES ({placeholders})",
                table.name(),
                columns.join(", "),
            );
            tx.execute(&sql, params_from_iter(bind))?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Sets one column on a live row.
    pub async fn set(&self, id: String, column: String, value: CrdtValue) -> Result<()> {
        validate_identifier("row_id", &id)?;
        reject_liveness_column(&column)?;
        validate_column(&self.table, &column, &value)?;
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let sql = format!(
                "UPDATE {} SET {column} = ?2 WHERE id = ?1 AND deleted = 0",
                table.name()
            );
            let changed = tx.execute(&sql, params![id, crdt_to_sqlite(&value)?])?;
            ensure_one_row(changed, "row not found or deleted")?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Reads the current value of one column on a live row.
    pub async fn get_cell(&self, id: String, column: String) -> Result<Option<CrdtValue>> {
        validate_identifier("row_id", &id)?;
        if self.table.column_type(&column).is_none() {
            return Err(SyncError::InvalidPayload(format!(
                "column is not synchronizable: {column}"
            )));
        }
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let live: Option<i64> = connection
                .query_row(
                    &format!(
                        "SELECT 1 FROM {} WHERE id = ?1 AND deleted = 0",
                        table.name()
                    ),
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            if live.is_none() {
                return Ok(None);
            }
            let raw: Option<String> = connection
                .query_row(
                    &format!(
                        "SELECT value FROM {} WHERE todo_id = ?1 AND column_name = ?2",
                        table.crdt_table()
                    ),
                    params![id, column],
                    |row| row.get(0),
                )
                .optional()?;
            raw.map(|value| decode_sqlite_value(&table, &column, &value))
                .transpose()
        })
        .await
    }

    /// Tombstones a live row. The deletion is a CRDT liveness write that
    /// converges with concurrent edits through the same LWW ordering.
    pub async fn delete(&self, id: String) -> Result<()> {
        validate_identifier("row_id", &id)?;
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let sql = format!(
                "UPDATE {} SET deleted = 1 WHERE id = ?1 AND deleted = 0",
                table.name()
            );
            let changed = tx.execute(&sql, params![id])?;
            ensure_one_row(changed, "row not found or already deleted")?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Restores a tombstoned row with a newer liveness version, so the
    /// restoration wins over the prior deletion under LWW ordering.
    pub async fn restore(&self, id: String) -> Result<()> {
        validate_identifier("row_id", &id)?;
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let sql = format!(
                "UPDATE {} SET deleted = 0 WHERE id = ?1 AND deleted = 1",
                table.name()
            );
            let changed = tx.execute(&sql, params![id])?;
            ensure_one_row(changed, "row not found or not deleted")?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    // --- todos convenience layer ----------------------------------------

    pub async fn create_todo(&self, id: String, title: String, completed: bool) -> Result<()> {
        validate_identifier("todo_id", &id)?;
        validate_column(&self.table, TITLE_COLUMN, &CrdtValue::Text(title.clone()))?;
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO todos(id, title, completed) VALUES (?1, ?2, ?3)",
                params![id, title, completed],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn update_title(&self, id: String, title: String) -> Result<()> {
        validate_identifier("todo_id", &id)?;
        validate_column(&self.table, TITLE_COLUMN, &CrdtValue::Text(title.clone()))?;
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = tx.execute(
                "UPDATE todos SET title = ?2 WHERE id = ?1",
                params![id, title],
            )?;
            ensure_one_row(changed, "todo not found during update_title")?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn update_completed(&self, id: String, completed: bool) -> Result<()> {
        validate_identifier("todo_id", &id)?;
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = tx.execute(
                "UPDATE todos SET completed = ?2 WHERE id = ?1",
                params![id, completed],
            )?;
            ensure_one_row(changed, "todo not found during update_completed")?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Tombstones a `todos` row. Alias of [`SqliteClient::delete`].
    pub async fn delete_todo(&self, id: String) -> Result<()> {
        self.delete(id).await
    }

    /// Restores a `todos` row. Alias of [`SqliteClient::restore`].
    pub async fn restore_todo(&self, id: String) -> Result<()> {
        self.restore(id).await
    }

    pub async fn get_todo(&self, id: String) -> Result<Option<Todo>> {
        validate_identifier("todo_id", &id)?;
        self.run(move |connection| {
            connection
                .query_row(
                    "SELECT id, title, completed FROM todos WHERE id = ?1 AND deleted = 0",
                    [id],
                    |row| {
                        Ok(Todo {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            completed: row.get(2)?,
                        })
                    },
                )
                .optional()
                .map_err(SyncError::from)
        })
        .await
    }

    // --- Synchronization -------------------------------------------------

    pub async fn local_delta(&self) -> Result<SyncPayload> {
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let payload = get_local_delta(&tx, &table)?;
            tx.commit()?;
            Ok(payload)
        })
        .await
    }

    pub async fn acknowledge(&self, payload: SyncPayload) -> Result<()> {
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            acknowledge_local_delta(&tx, &payload, &table)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn apply_remote(&self, payload: SyncPayload) -> Result<()> {
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            apply_remote_payload(&tx, &payload, &table)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Atomically acknowledges the sent versions and applies the server response.
    pub async fn complete_sync(&self, sent: SyncPayload, response: SyncPayload) -> Result<()> {
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            acknowledge_local_delta(&tx, &sent, &table)?;
            apply_remote_payload(&tx, &response, &table)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Executes a complete synchronization cycle through a caller-provided transport.
    ///
    /// The callback runs without holding the `SQLite` mutex. Transport failure leaves all
    /// local changes dirty and retryable.
    pub async fn sync_with<F, Fut>(&self, transport: F) -> Result<SyncPayload>
    where
        F: FnOnce(SyncPayload) -> Fut,
        Fut: Future<Output = Result<SyncPayload>>,
    {
        let outbound = self.local_delta().await?;
        let response = transport(outbound.clone()).await?;
        self.complete_sync(outbound, response.clone()).await?;
        Ok(response)
    }

    /// Repeats bounded sync pages until the server reports that the client is
    /// caught up. Each page is acknowledged and applied atomically before the
    /// next request, so interruption remains safely retryable.
    pub async fn sync_until_caught_up<F, Fut>(&self, mut transport: F) -> Result<SyncPayload>
    where
        F: FnMut(SyncPayload) -> Fut,
        Fut: Future<Output = Result<SyncPayload>>,
    {
        for _ in 0..MAX_SYNC_PAGES_PER_CALL {
            let outbound = self.local_delta().await?;
            let response = transport(outbound.clone()).await?;
            self.complete_sync(outbound, response.clone()).await?;
            if !response.has_more {
                return Ok(response);
            }
        }
        Err(SyncError::SyncPageBudgetExhausted)
    }

    /// Builds one authoritative partial-replica request. Creating the request
    /// advances the scope revision atomically, so a later concurrent request
    /// makes this request's eventual response stale and therefore harmless.
    pub async fn partial_replica_request(
        &self,
        scope_id: String,
        interest: ReplicaInterest,
    ) -> Result<PartialReplicaRequest> {
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let request = get_partial_replica_request(&tx, &table, &scope_id, interest)?;
            tx.commit()?;
            Ok(request)
        })
        .await
    }

    /// Atomically acknowledges a partial-replica request, applies its complete
    /// authoritative snapshot, updates scope membership, and performs safe
    /// local eviction.
    pub async fn complete_partial_replica_sync(
        &self,
        sent: PartialReplicaRequest,
        response: PartialReplicaResponse,
    ) -> Result<()> {
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            apply_partial_replica_response(&tx, &sent, &response, &table)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Unsubscribes a partial-replica scope and safely evicts rows no longer
    /// referenced by another scope. Its retained revision invalidates any
    /// response already in flight when the scope is removed.
    pub async fn remove_partial_replica_scope(&self, scope_id: String) -> Result<()> {
        let table = Arc::clone(&self.table);
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            remove_partial_replica_scope(&tx, &table, &scope_id)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Executes one complete authoritative partial-replica synchronization.
    /// Network I/O runs without holding the `SQLite` mutex.
    pub async fn sync_partial_with<F, Fut>(
        &self,
        scope_id: String,
        interest: ReplicaInterest,
        transport: F,
    ) -> Result<PartialReplicaResponse>
    where
        F: FnOnce(PartialReplicaRequest) -> Fut,
        Fut: Future<Output = Result<PartialReplicaResponse>>,
    {
        let outbound = self.partial_replica_request(scope_id, interest).await?;
        let response = transport(outbound.clone()).await?;
        self.complete_partial_replica_sync(outbound, response.clone())
            .await?;
        Ok(response)
    }

    /// Resets this table's local cursor so the next sync performs a full,
    /// bounded repair. Local dirty writes are preserved.
    pub async fn reset_cursor(&self) -> Result<()> {
        let crdt_table = self.table.crdt_table();
        self.run(move |connection| {
            connection.execute(
                "INSERT INTO loomabase_cursor(crdt_table, cursor, cursor_token, server_epoch)
                 VALUES (?1, 0, NULL, NULL)
                 ON CONFLICT(crdt_table) DO UPDATE SET
                    cursor = 0,
                    cursor_token = NULL,
                    server_epoch = NULL",
                [crdt_table],
            )?;
            Ok(())
        })
        .await
    }

    async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let mut guard = lock_connection(&connection)?;
            operation(&mut guard)
        })
        .await
        .map_err(|error| SyncError::BlockingTask(error.to_string()))?
    }
}

fn lock_connection(connection: &Mutex<Connection>) -> Result<MutexGuard<'_, Connection>> {
    connection.lock().map_err(|_| SyncError::SqliteLockPoisoned)
}

/// Initializes a client database for the canonical `todos` contract.
pub fn initialize_client(connection: &mut Connection, device_id: &str) -> Result<()> {
    initialize_client_with(connection, device_id, &todos_table())
}

/// Initializes a client database for a single-table contract.
pub fn initialize_client_with(
    connection: &mut Connection,
    device_id: &str,
    table: &TableDef,
) -> Result<()> {
    let contract = Contract::new(vec![table.clone()])?;
    initialize_client_with_contract(connection, device_id, &contract)
}

/// Initializes a client database for a multi-table synchronization contract.
/// Every table shares the device clock and the same edge database.
pub fn initialize_client_with_contract(
    connection: &mut Connection,
    device_id: &str,
    contract: &Contract,
) -> Result<()> {
    validate_identifier("device_id", device_id)?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;",
    )?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    // Migrate any existing tables up to the contract before (re)generating the
    // schema. On a fresh database there are no columns and this is a no-op.
    for table in contract.tables() {
        let existing = sqlite_existing_columns(&tx, table.name())?;
        if !existing.is_empty() {
            for statement in table.sqlite_migration_sql(&existing)? {
                tx.execute_batch(&statement)?;
            }
        }
    }
    tx.execute_batch(&contract.sqlite_schema())?;
    migrate_client_metadata(&tx)?;

    let current_device: Option<String> = tx
        .query_row(
            "SELECT device_id FROM client_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match current_device {
        None => {
            tx.execute(
                "INSERT INTO client_state(singleton, device_id, lamport_clock, applying_remote)
                 VALUES (1, ?1, 0, 0)",
                [device_id],
            )?;
        }
        Some(current) if current == device_id => {}
        Some(_) => {
            return Err(SyncError::InvalidPayload(
                "the local database belongs to a different device".to_owned(),
            ));
        }
    }
    tx.commit()?;
    Ok(())
}

/// Reads the existing column names and declared types of a table, or an empty
/// map when the table does not yet exist.
fn sqlite_existing_columns(
    tx: &Transaction<'_>,
    table_name: &str,
) -> Result<BTreeMap<String, String>> {
    let mut statement = tx.prepare(&format!("PRAGMA table_info({table_name})"))?;
    let mut rows = statement.query([])?;
    let mut columns = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        let column_type: String = row.get(2)?;
        columns.insert(name, column_type);
    }
    Ok(columns)
}

fn migrate_client_metadata(tx: &Transaction<'_>) -> Result<()> {
    let cursor_columns = sqlite_existing_columns(tx, "loomabase_cursor")?;
    if !cursor_columns.is_empty() {
        if !cursor_columns.contains_key("cursor_token") {
            tx.execute_batch("ALTER TABLE loomabase_cursor ADD COLUMN cursor_token TEXT")?;
        }
        if !cursor_columns.contains_key("server_epoch") {
            tx.execute_batch("ALTER TABLE loomabase_cursor ADD COLUMN server_epoch TEXT")?;
        }
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS loomabase_scope_state (
            scope_id TEXT NOT NULL,
            crdt_table TEXT NOT NULL,
            version INTEGER NOT NULL CHECK (version > 0),
            active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
            PRIMARY KEY (scope_id, crdt_table)
         );
         CREATE TABLE IF NOT EXISTS loomabase_scope_members (
            scope_id TEXT NOT NULL,
            crdt_table TEXT NOT NULL,
            row_id TEXT NOT NULL,
            PRIMARY KEY (scope_id, crdt_table, row_id)
         );
         CREATE INDEX IF NOT EXISTS loomabase_scope_members_row_idx
            ON loomabase_scope_members(crdt_table, row_id);
         CREATE TABLE IF NOT EXISTS loomabase_scope_evictions (
            scope_id TEXT NOT NULL,
            crdt_table TEXT NOT NULL,
            row_id TEXT NOT NULL,
            PRIMARY KEY (scope_id, crdt_table, row_id)
         );
         CREATE INDEX IF NOT EXISTS loomabase_scope_evictions_row_idx
            ON loomabase_scope_evictions(crdt_table, row_id);",
    )?;
    let scope_state_columns = sqlite_existing_columns(tx, "loomabase_scope_state")?;
    if !scope_state_columns.contains_key("active") {
        tx.execute_batch(
            "ALTER TABLE loomabase_scope_state
             ADD COLUMN active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1))",
        )?;
    }
    Ok(())
}

/// Extracts the dirty delta and the table's change-feed cursor from the same
/// transactional snapshot.
pub fn get_local_delta(tx: &Transaction<'_>, table: &TableDef) -> Result<SyncPayload> {
    let (device_id, source_lamport): (String, i64) = tx.query_row(
        "SELECT device_id, lamport_clock FROM client_state WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let crdt = table.crdt_table();
    let (cursor, cursor_token, server_epoch): (i64, Option<String>, Option<String>) = tx
        .query_row(
            "SELECT cursor, cursor_token, server_epoch
             FROM loomabase_cursor WHERE crdt_table = ?1",
            [&crdt],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .unwrap_or((0, None, None));

    let mut changes_by_row: BTreeMap<String, BTreeMap<String, CrdtColumn>> = BTreeMap::new();
    let mut statement = tx.prepare(&format!(
        "SELECT todo_id, column_name, value, lamport_clock, device_id
         FROM {crdt} WHERE dirty = 1 ORDER BY todo_id, column_name"
    ))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let todo_id: String = row.get(0)?;
        let column_name: String = row.get(1)?;
        let raw_value: String = row.get(2)?;
        let lamport_clock: i64 = row.get(3)?;
        let metadata_device_id: String = row.get(4)?;
        let value = decode_sqlite_value(table, &column_name, &raw_value)?;
        changes_by_row.entry(todo_id).or_default().insert(
            column_name,
            CrdtColumn {
                value,
                metadata: ColumnMetadata {
                    lamport_clock: clock_from_i64(lamport_clock)?,
                    device_id: metadata_device_id,
                },
            },
        );
    }

    let payload = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: table.fingerprint(),
        source_device_id: device_id,
        source_lamport: clock_from_i64(source_lamport)?,
        changes: changes_by_row
            .into_iter()
            .map(|(todo_id, columns)| RowChange { todo_id, columns })
            .collect(),
        cursor,
        has_more: false,
        cursor_reset: false,
        cursor_token,
        server_epoch,
    };
    payload.validate_client_request(&payload.source_device_id, table)?;
    Ok(payload)
}

/// Acknowledges only the versions that were sent; concurrent writes remain dirty.
pub fn acknowledge_local_delta(
    tx: &Transaction<'_>,
    payload: &SyncPayload,
    table: &TableDef,
) -> Result<()> {
    let local_device_id: String = tx.query_row(
        "SELECT device_id FROM client_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    payload.validate_client_request(&local_device_id, table)?;
    let sql = format!(
        "UPDATE {} SET dirty = 0
         WHERE todo_id = ?1 AND column_name = ?2
           AND lamport_clock = ?3 AND device_id = ?4",
        table.crdt_table()
    );
    for row in &payload.changes {
        for (column_name, column) in &row.columns {
            tx.execute(
                &sql,
                params![
                    row.todo_id,
                    column_name,
                    clock_to_i64(column.metadata.lamport_clock)?,
                    column.metadata.device_id
                ],
            )?;
        }
    }
    cleanup_pending_evictions(tx, table)?;
    Ok(())
}

/// Creates a revisioned request containing the local dirty delta and the
/// client's current authoritative membership for one scope.
pub fn get_partial_replica_request(
    tx: &Transaction<'_>,
    table: &TableDef,
    scope_id: &str,
    interest: ReplicaInterest,
) -> Result<PartialReplicaRequest> {
    validate_identifier("scope_id", scope_id)?;
    interest.validate(table)?;
    let crdt_table = table.crdt_table();
    let current_version: Option<i64> = tx
        .query_row(
            "SELECT version FROM loomabase_scope_state
             WHERE scope_id = ?1 AND crdt_table = ?2",
            params![scope_id, crdt_table],
            |row| row.get(0),
        )
        .optional()?;
    let scope_version = current_version
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(SyncError::ClockOverflow)?;
    tx.execute(
        "INSERT INTO loomabase_scope_state(scope_id, crdt_table, version, active)
         VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(scope_id, crdt_table) DO UPDATE SET
            version = excluded.version,
            active = 1",
        params![scope_id, crdt_table, scope_version],
    )?;

    let mut statement = tx.prepare(
        "SELECT row_id FROM loomabase_scope_members
         WHERE scope_id = ?1 AND crdt_table = ?2 ORDER BY row_id",
    )?;
    let known_member_ids = statement
        .query_map(params![scope_id, crdt_table], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, rusqlite::Error>>()?;
    let request = PartialReplicaRequest {
        scope_id: scope_id.to_owned(),
        scope_version: clock_from_i64(scope_version)?,
        interest,
        known_member_ids,
        sync: get_local_delta(tx, table)?,
    };
    request.validate(table, &request.sync.source_device_id)?;
    Ok(request)
}

/// Applies a complete authoritative membership snapshot. Scope removal is a
/// local storage concern and never creates a replicated tombstone.
pub fn apply_partial_replica_response(
    tx: &Transaction<'_>,
    sent: &PartialReplicaRequest,
    response: &PartialReplicaResponse,
    table: &TableDef,
) -> Result<()> {
    let local_device_id: String = tx.query_row(
        "SELECT device_id FROM client_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    sent.validate(table, &local_device_id)?;
    response.validate(table)?;
    if sent.scope_id != response.scope_id || sent.scope_version != response.scope_version {
        return Err(SyncError::InvalidPayload(
            "partial replica response does not match the request scope revision".to_owned(),
        ));
    }
    let crdt_table = table.crdt_table();
    let current_state: Option<(i64, bool)> = tx
        .query_row(
            "SELECT version, active FROM loomabase_scope_state
             WHERE scope_id = ?1 AND crdt_table = ?2",
            params![sent.scope_id, crdt_table],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if current_state != Some((clock_to_i64(sent.scope_version)?, true)) {
        return Err(SyncError::InvalidPayload(
            "stale partial replica response rejected".to_owned(),
        ));
    }

    let expected_evictions = sent
        .known_member_ids
        .iter()
        .filter(|row_id| response.member_ids.binary_search(row_id).is_err())
        .cloned()
        .collect::<Vec<_>>();
    if expected_evictions != response.evicted_row_ids {
        return Err(SyncError::InvalidPayload(
            "partial replica response contains an inconsistent eviction set".to_owned(),
        ));
    }

    acknowledge_local_delta(tx, &sent.sync, table)?;
    apply_remote_payload(tx, &response.sync, table)?;

    tx.execute(
        "DELETE FROM loomabase_scope_members
         WHERE scope_id = ?1 AND crdt_table = ?2",
        params![sent.scope_id, crdt_table],
    )?;
    for row_id in &response.member_ids {
        tx.execute(
            "INSERT INTO loomabase_scope_members(scope_id, crdt_table, row_id)
             VALUES (?1, ?2, ?3)",
            params![sent.scope_id, crdt_table, row_id],
        )?;
        tx.execute(
            "DELETE FROM loomabase_scope_evictions
             WHERE scope_id = ?1 AND crdt_table = ?2 AND row_id = ?3",
            params![sent.scope_id, crdt_table, row_id],
        )?;
    }
    for row_id in &response.evicted_row_ids {
        tx.execute(
            "INSERT INTO loomabase_scope_evictions(scope_id, crdt_table, row_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_id, crdt_table, row_id) DO NOTHING",
            params![sent.scope_id, crdt_table, row_id],
        )?;
    }
    cleanup_pending_evictions(tx, table)?;
    Ok(())
}

/// Unsubscribes one scope while preserving an inactive monotonic revision that
/// invalidates every response already in flight.
pub fn remove_partial_replica_scope(
    tx: &Transaction<'_>,
    table: &TableDef,
    scope_id: &str,
) -> Result<()> {
    validate_identifier("scope_id", scope_id)?;
    let crdt_table = table.crdt_table();
    let current_version: Option<i64> = tx
        .query_row(
            "SELECT version FROM loomabase_scope_state
             WHERE scope_id = ?1 AND crdt_table = ?2",
            params![scope_id, crdt_table],
            |row| row.get(0),
        )
        .optional()?;
    let next_version = current_version
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(SyncError::ClockOverflow)?;
    tx.execute(
        "INSERT INTO loomabase_scope_state(scope_id, crdt_table, version, active)
         VALUES (?1, ?2, ?3, 0)
         ON CONFLICT(scope_id, crdt_table) DO UPDATE SET
            version = excluded.version,
            active = 0",
        params![scope_id, crdt_table, next_version],
    )?;

    let mut statement = tx.prepare(
        "SELECT row_id FROM loomabase_scope_members
         WHERE scope_id = ?1 AND crdt_table = ?2 ORDER BY row_id",
    )?;
    let members = statement
        .query_map(params![scope_id, crdt_table], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, rusqlite::Error>>()?;
    drop(statement);
    tx.execute(
        "DELETE FROM loomabase_scope_members
         WHERE scope_id = ?1 AND crdt_table = ?2",
        params![scope_id, crdt_table],
    )?;
    for row_id in members {
        tx.execute(
            "INSERT INTO loomabase_scope_evictions(scope_id, crdt_table, row_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_id, crdt_table, row_id) DO NOTHING",
            params![scope_id, crdt_table, row_id],
        )?;
    }
    cleanup_pending_evictions(tx, table)?;
    Ok(())
}

/// Applies a remote response without firing local-change triggers.
pub fn apply_remote_payload(
    tx: &Transaction<'_>,
    payload: &SyncPayload,
    table: &TableDef,
) -> Result<()> {
    payload.validate_server_response(table)?;
    let current_clock: i64 = tx.query_row(
        "SELECT lamport_clock FROM client_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let current_clock = clock_from_i64(current_clock)?;
    tx.execute(
        "UPDATE client_state SET applying_remote = 1 WHERE singleton = 1",
        [],
    )?;

    let insert_row = format!(
        "INSERT INTO {}(id) VALUES (?1) ON CONFLICT(id) DO NOTHING",
        table.name()
    );
    let upsert_cell = format!(
        "INSERT INTO {}
            (todo_id, column_name, value, lamport_clock, device_id, dirty)
         VALUES (?1, ?2, ?3, ?4, ?5, 0)
         ON CONFLICT(todo_id, column_name) DO UPDATE SET
            value = excluded.value,
            lamport_clock = excluded.lamport_clock,
            device_id = excluded.device_id,
            dirty = 0",
        table.crdt_table()
    );

    for row in &payload.changes {
        tx.execute(&insert_row, [&row.todo_id])?;
        for (column_name, incoming) in &row.columns {
            let current = read_local_crdt_column(tx, table, &row.todo_id, column_name)?;
            let should_apply = match current {
                None => true,
                Some(current) => match decide_lww(&current.metadata, &incoming.metadata) {
                    MergeDecision::AcceptIncoming => true,
                    MergeDecision::KeepCurrent => false,
                    MergeDecision::Equal if current.value == incoming.value => false,
                    MergeDecision::Equal => {
                        return Err(SyncError::InvalidPayload(
                            "the same CRDT version cannot identify different values".to_owned(),
                        ));
                    }
                },
            };
            if should_apply {
                write_todo_column(tx, table, &row.todo_id, column_name, &incoming.value)?;
                tx.execute(
                    &upsert_cell,
                    params![
                        row.todo_id,
                        column_name,
                        encode_sqlite_value(&incoming.value)?,
                        clock_to_i64(incoming.metadata.lamport_clock)?,
                        incoming.metadata.device_id
                    ],
                )?;
            }
        }
    }

    let next_clock = current_clock
        .max(payload.max_observed_clock())
        .checked_add(1)
        .ok_or(SyncError::ClockOverflow)?;
    tx.execute(
        "UPDATE client_state SET lamport_clock = ?1, applying_remote = 0 WHERE singleton = 1",
        [clock_to_i64(next_clock)?],
    )?;
    if payload.cursor_reset {
        tx.execute(
            "INSERT INTO loomabase_cursor(crdt_table, cursor, cursor_token, server_epoch)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(crdt_table) DO UPDATE SET
                cursor = excluded.cursor,
                cursor_token = excluded.cursor_token,
                server_epoch = excluded.server_epoch",
            params![
                table.crdt_table(),
                payload.cursor,
                payload.cursor_token,
                payload.server_epoch
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO loomabase_cursor(crdt_table, cursor, cursor_token, server_epoch)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(crdt_table) DO UPDATE SET
                cursor = MAX(cursor, excluded.cursor),
                cursor_token = excluded.cursor_token,
                server_epoch = excluded.server_epoch",
            params![
                table.crdt_table(),
                payload.cursor,
                payload.cursor_token,
                payload.server_epoch
            ],
        )?;
    }
    Ok(())
}

/// Removes rows that no longer belong to any local scope once every local
/// write has been acknowledged. Pending markers make eviction retryable across
/// crashes and preserve dirty offline writes until they reach the server.
fn cleanup_pending_evictions(tx: &Transaction<'_>, table: &TableDef) -> Result<()> {
    let crdt_table = table.crdt_table();
    let mut statement = tx.prepare(
        "SELECT DISTINCT row_id FROM loomabase_scope_evictions
         WHERE crdt_table = ?1 ORDER BY row_id",
    )?;
    let pending = statement
        .query_map([&crdt_table], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, rusqlite::Error>>()?;
    drop(statement);

    for row_id in pending {
        let membership_exists: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM loomabase_scope_members
                WHERE crdt_table = ?1 AND row_id = ?2
             )",
            params![crdt_table, row_id],
            |row| row.get(0),
        )?;
        if membership_exists {
            tx.execute(
                "DELETE FROM loomabase_scope_evictions
                 WHERE crdt_table = ?1 AND row_id = ?2",
                params![crdt_table, row_id],
            )?;
            continue;
        }
        let dirty_exists: bool = tx.query_row(
            &format!(
                "SELECT EXISTS(
                    SELECT 1 FROM {crdt_table} WHERE todo_id = ?1 AND dirty = 1
                 )"
            ),
            [&row_id],
            |row| row.get(0),
        )?;
        if dirty_exists {
            continue;
        }

        tx.execute(
            "UPDATE client_state SET applying_remote = 1 WHERE singleton = 1",
            [],
        )?;
        tx.execute(
            &format!("DELETE FROM {} WHERE id = ?1", table.name()),
            [&row_id],
        )?;
        tx.execute(
            "UPDATE client_state SET applying_remote = 0 WHERE singleton = 1",
            [],
        )?;
        tx.execute(
            "DELETE FROM loomabase_scope_evictions
             WHERE crdt_table = ?1 AND row_id = ?2",
            params![crdt_table, row_id],
        )?;
    }
    Ok(())
}

pub fn read_todo(connection: &Connection, id: &str) -> Result<Option<(String, bool)>> {
    connection
        .query_row(
            "SELECT title, completed FROM todos WHERE id = ?1 AND deleted = 0",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(SyncError::from)
}

fn read_local_crdt_column(
    tx: &Transaction<'_>,
    table: &TableDef,
    todo_id: &str,
    column_name: &str,
) -> Result<Option<CrdtColumn>> {
    let raw: Option<(String, i64, String)> = tx
        .query_row(
            &format!(
                "SELECT value, lamport_clock, device_id FROM {}
                 WHERE todo_id = ?1 AND column_name = ?2",
                table.crdt_table()
            ),
            params![todo_id, column_name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    raw.map(|(value, clock, device_id)| {
        Ok(CrdtColumn {
            value: decode_sqlite_value(table, column_name, &value)?,
            metadata: ColumnMetadata {
                lamport_clock: clock_from_i64(clock)?,
                device_id,
            },
        })
    })
    .transpose()
}

fn write_todo_column(
    tx: &Transaction<'_>,
    table: &TableDef,
    todo_id: &str,
    column_name: &str,
    value: &CrdtValue,
) -> Result<()> {
    let Some(ty) = table.column_type(column_name) else {
        return Err(SyncError::InvalidPayload(format!(
            "column is not synchronizable: {column_name}"
        )));
    };
    let typed_match = matches!(
        (ty, value),
        (ColumnType::Text, CrdtValue::Text(_))
            | (ColumnType::Integer, CrdtValue::Integer(_))
            | (ColumnType::Real, CrdtValue::Real(_))
            | (ColumnType::Boolean, CrdtValue::Boolean(_))
    );
    if !typed_match {
        return Err(SyncError::InvalidPayload(format!(
            "value type is incompatible with column {column_name}"
        )));
    }
    let sql = format!(
        "UPDATE {} SET {column_name} = ?2 WHERE id = ?1",
        table.name()
    );
    tx.execute(&sql, params![todo_id, crdt_to_sqlite(value)?])?;
    Ok(())
}

fn decode_sqlite_value(table: &TableDef, column_name: &str, raw_value: &str) -> Result<CrdtValue> {
    let Some(ty) = table.column_type(column_name) else {
        return Err(SyncError::InvalidPayload(format!(
            "column is not synchronizable: {column_name}"
        )));
    };
    match ty {
        ColumnType::Text => Ok(CrdtValue::Text(raw_value.to_owned())),
        ColumnType::Integer => raw_value
            .parse::<i64>()
            .map(CrdtValue::Integer)
            .map_err(|_| {
                SyncError::InvalidPayload("non-integer value in integer column".to_owned())
            }),
        ColumnType::Real => raw_value
            .parse::<f64>()
            .map(CrdtValue::Real)
            .map_err(|_| SyncError::InvalidPayload("non-real value in real column".to_owned())),
        ColumnType::Boolean => match raw_value {
            "0" => Ok(CrdtValue::Boolean(false)),
            "1" => Ok(CrdtValue::Boolean(true)),
            _ => Err(SyncError::InvalidPayload(
                "non-canonical SQLite boolean".to_owned(),
            )),
        },
    }
}

fn encode_sqlite_value(value: &CrdtValue) -> Result<String> {
    match value {
        CrdtValue::Text(value) => Ok(value.clone()),
        CrdtValue::Integer(value) => Ok(value.to_string()),
        CrdtValue::Real(value) => Ok(value.to_string()),
        CrdtValue::Boolean(value) => Ok(i32::from(*value).to_string()),
        CrdtValue::Null | CrdtValue::Blob(_) => Err(SyncError::InvalidPayload(
            "value type cannot be stored in the SQLite edge schema".to_owned(),
        )),
    }
}

fn crdt_to_sqlite(value: &CrdtValue) -> Result<SqlValue> {
    Ok(match value {
        CrdtValue::Text(value) => SqlValue::Text(value.clone()),
        CrdtValue::Integer(value) => SqlValue::Integer(*value),
        CrdtValue::Real(value) => SqlValue::Real(*value),
        CrdtValue::Boolean(value) => SqlValue::Integer(i64::from(*value)),
        CrdtValue::Null | CrdtValue::Blob(_) => {
            return Err(SyncError::InvalidPayload(
                "value type cannot be stored in the SQLite edge schema".to_owned(),
            ));
        }
    })
}

fn clock_from_i64(clock: i64) -> Result<u64> {
    u64::try_from(clock)
        .map_err(|_| SyncError::InvalidPayload("negative clock stored in database".to_owned()))
}

fn clock_to_i64(clock: u64) -> Result<i64> {
    i64::try_from(clock).map_err(|_| SyncError::ClockOverflow)
}

fn reject_liveness_column(column: &str) -> Result<()> {
    if column == LIVENESS_COLUMN {
        Err(SyncError::InvalidPayload(
            "use delete() or restore() to change row liveness".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn ensure_one_row(changed: usize, message: &str) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(SyncError::InvalidPayload(message.to_owned()))
    }
}
