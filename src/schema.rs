//! Declarative sync contract and SQL generation.
//!
//! A [`TableDef`] describes one synchronized table. The `SQLite` and `PostgreSQL`
//! schema, indexes, and `SQLite` change-capture triggers are generated from that
//! declaration instead of being hand-written, so adding a synchronized column
//! is a single declaration rather than an edit spread across the codebase.
//!
//! Every table carries an implicit `id TEXT` primary key and a reserved
//! [`LIVENESS_COLUMN`] (`deleted`) liveness register. Contract identifiers are
//! developer-controlled, never payload-controlled, but they are still validated
//! as strict lowercase SQL identifiers before being interpolated into DDL.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::crdt::{COMPLETED_COLUMN, DELETED_COLUMN, TITLE_COLUMN};
use crate::error::{Result, SyncError};

/// Reserved per-row liveness register written by create, delete, and restore.
pub const LIVENESS_COLUMN: &str = DELETED_COLUMN;
/// Reserved primary-key column present on every synchronized table.
pub const PRIMARY_KEY_COLUMN: &str = "id";
const MAX_IDENTIFIER_LEN: usize = 63;
const I64_MAX_LITERAL: &str = "9223372036854775807";

/// The SQL value domain a synchronized column materializes into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColumnType {
    Text,
    Integer,
    Real,
    Boolean,
}

impl ColumnType {
    fn sqlite_type(self) -> &'static str {
        match self {
            ColumnType::Text => "TEXT",
            ColumnType::Integer => "INTEGER",
            ColumnType::Real => "REAL",
            ColumnType::Boolean => "BOOLEAN",
        }
    }

    fn postgres_type(self) -> &'static str {
        match self {
            ColumnType::Text => "TEXT",
            ColumnType::Integer => "BIGINT",
            ColumnType::Real => "DOUBLE PRECISION",
            ColumnType::Boolean => "BOOLEAN",
        }
    }

    fn sqlite_default(self) -> &'static str {
        match self {
            ColumnType::Text => "''",
            ColumnType::Integer | ColumnType::Real | ColumnType::Boolean => "0",
        }
    }

    fn postgres_default(self) -> &'static str {
        match self {
            ColumnType::Text => "''",
            ColumnType::Integer | ColumnType::Real => "0",
            ColumnType::Boolean => "FALSE",
        }
    }

    /// Stable, version-independent tag used in the contract fingerprint.
    fn tag(self) -> &'static str {
        match self {
            ColumnType::Text => "text",
            ColumnType::Integer => "integer",
            ColumnType::Real => "real",
            ColumnType::Boolean => "boolean",
        }
    }

    /// The `SQLite` expression that captures a `NEW` row value into the CRDT log.
    /// Text is already textual; other domains are cast to their textual form.
    fn sqlite_capture_expr(self, column: &str) -> String {
        match self {
            ColumnType::Text => format!("NEW.{column}"),
            ColumnType::Integer | ColumnType::Real | ColumnType::Boolean => {
                format!("CAST(NEW.{column} AS TEXT)")
            }
        }
    }
}

/// One synchronized, application-visible column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// A declarative description of one synchronized table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableDef {
    name: String,
    columns: Vec<ColumnDef>,
}

impl TableDef {
    /// Builds a validated table definition. The reserved `id` and `deleted`
    /// columns are implicit and must not appear in `columns`.
    ///
    /// # Errors
    /// Returns [`SyncError::InvalidPayload`] when the table or any column name
    /// is not a strict lowercase SQL identifier, collides with a reserved name,
    /// is duplicated, or when no application column is declared.
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> Result<Self> {
        let name = name.into();
        validate_sql_identifier("table name", &name)?;
        if matches!(
            name.as_str(),
            "client_state"
                | "loomabase_cursor"
                | "loomabase_cursor_lease"
                | "loomabase_server_state"
                | "loomabase_state"
        ) {
            return Err(SyncError::InvalidPayload(format!(
                "table name {name} is reserved"
            )));
        }
        if columns.is_empty() {
            return Err(SyncError::InvalidPayload(
                "a synchronized table needs at least one application column".to_owned(),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        for column in &columns {
            validate_sql_identifier("column name", &column.name)?;
            if column.name == PRIMARY_KEY_COLUMN || column.name == LIVENESS_COLUMN {
                return Err(SyncError::InvalidPayload(format!(
                    "column name {} is reserved",
                    column.name
                )));
            }
            if !seen.insert(column.name.clone()) {
                return Err(SyncError::InvalidPayload(format!(
                    "duplicate column {}",
                    column.name
                )));
            }
        }

        Ok(Self { name, columns })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Application columns, excluding the reserved primary key and liveness.
    #[must_use]
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// Name of the CRDT metadata log table for this table.
    #[must_use]
    pub fn crdt_table(&self) -> String {
        format!("{}_crdt", self.name)
    }

    /// Application columns plus the reserved liveness register, in capture order.
    fn synchronized_columns(&self) -> Vec<ColumnDef> {
        let mut columns = self.columns.clone();
        columns.push(ColumnDef::new(LIVENESS_COLUMN, ColumnType::Boolean));
        columns
    }

    /// Names of every column carried in the CRDT protocol for this table.
    #[must_use]
    pub fn synchronized_column_names(&self) -> Vec<String> {
        self.synchronized_columns()
            .into_iter()
            .map(|column| column.name)
            .collect()
    }

    /// The type a synchronized column materializes into, if it is part of the
    /// contract (including the reserved liveness column).
    #[must_use]
    pub fn column_type(&self, name: &str) -> Option<ColumnType> {
        if name == LIVENESS_COLUMN {
            return Some(ColumnType::Boolean);
        }
        self.columns
            .iter()
            .find(|column| column.name == name)
            .map(|column| column.ty)
    }

    /// Stable contract fingerprint for the schema handshake. It is deterministic
    /// across processes and platforms and changes whenever the table name, its
    /// columns, or any column type changes, so an incompatible client and server
    /// can be detected before any mutation.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut descriptor = format!("loomabase-contract-v1;{}", self.name);
        for column in self.synchronized_columns() {
            let _ = write!(descriptor, ";{}:{}", column.name, column.ty.tag());
        }
        fnv1a_64(descriptor.as_bytes())
    }

    /// Statements that migrate an existing `SQLite` table whose synchronized
    /// columns are `existing` (name -> declared type) up to this contract. Only
    /// additive changes are automatic; a retyped column is destructive and
    /// returns an error. Missing columns are added and the insert trigger is
    /// dropped so the regenerated schema recreates it capturing the new columns.
    ///
    /// # Errors
    /// Returns an error if a column exists with a type incompatible with the
    /// contract.
    pub fn sqlite_migration_sql(&self, existing: &BTreeMap<String, String>) -> Result<Vec<String>> {
        reject_removed_columns(
            existing,
            std::iter::once(PRIMARY_KEY_COLUMN).chain(
                self.synchronized_columns()
                    .iter()
                    .map(|column| column.name.as_str()),
            ),
        )?;
        let mut statements = Vec::new();
        let mut added = false;
        for column in self.synchronized_columns() {
            match existing.get(&column.name) {
                None => {
                    statements.push(format!(
                        "ALTER TABLE {} ADD COLUMN {}",
                        self.name,
                        sqlite_column_ddl(&column)
                    ));
                    added = true;
                }
                Some(declared) if !declared.eq_ignore_ascii_case(column.ty.sqlite_type()) => {
                    return Err(incompatible_type_change(&column.name));
                }
                Some(_) => {}
            }
        }
        if added {
            statements.push(format!(
                "DROP TRIGGER IF EXISTS {}_crdt_after_insert",
                self.name
            ));
        }
        Ok(statements)
    }

    /// Statements that migrate an existing `PostgreSQL` table whose synchronized
    /// columns are `existing` (name -> data type) up to this contract.
    ///
    /// # Errors
    /// Returns an error if a column exists with a type incompatible with the
    /// contract.
    pub fn postgres_migration_sql(
        &self,
        existing: &BTreeMap<String, String>,
    ) -> Result<Vec<String>> {
        reject_removed_columns(
            existing,
            ["tenant_id", PRIMARY_KEY_COLUMN].into_iter().chain(
                self.synchronized_columns()
                    .iter()
                    .map(|column| column.name.as_str()),
            ),
        )?;
        let mut statements = Vec::new();
        for column in self.synchronized_columns() {
            match existing.get(&column.name) {
                None => statements.push(format!(
                    "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {}",
                    self.name,
                    postgres_column_ddl(&column)
                )),
                Some(declared) if !declared.eq_ignore_ascii_case(column.ty.postgres_type()) => {
                    return Err(incompatible_type_change(&column.name));
                }
                Some(_) => {}
            }
        }
        Ok(statements)
    }

    /// Migrates the server CRDT metadata table to the current change-feed
    /// layout. Existing cells receive positive sequence numbers so a cursor-0
    /// repair includes them after upgrading from a pre-cursor release.
    pub fn postgres_crdt_migration_sql(
        &self,
        existing: &BTreeMap<String, String>,
    ) -> Result<Vec<String>> {
        if existing.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(declared) = existing.get("seq")
            && !declared.eq_ignore_ascii_case("bigint")
        {
            return Err(incompatible_type_change("seq"));
        }
        let crdt = self.crdt_table();
        Ok(vec![
            format!("ALTER TABLE {crdt} ADD COLUMN IF NOT EXISTS seq BIGINT"),
            format!("UPDATE {crdt} SET seq = nextval('loomabase_seq') WHERE seq IS NULL"),
            format!("ALTER TABLE {crdt} ALTER COLUMN seq SET NOT NULL"),
            format!("ALTER TABLE {crdt} ALTER COLUMN seq SET DEFAULT nextval('loomabase_seq')"),
        ])
    }

    /// Generates the full `SQLite` client schema: fixed device state, the
    /// application table, the CRDT log, its index, and the change-capture
    /// triggers. Equivalent to the previously hand-written schema for `todos`.
    #[must_use]
    pub fn sqlite_schema(&self) -> String {
        let crdt = self.crdt_table();
        let synchronized = self.synchronized_columns();

        let mut schema = String::from(
            "
CREATE TABLE IF NOT EXISTS client_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    device_id TEXT NOT NULL,
    lamport_clock INTEGER NOT NULL CHECK (lamport_clock >= 0),
    applying_remote INTEGER NOT NULL DEFAULT 0 CHECK (applying_remote IN (0, 1))
);
CREATE TABLE IF NOT EXISTS loomabase_cursor (
    crdt_table TEXT PRIMARY KEY,
    cursor INTEGER NOT NULL DEFAULT 0,
    cursor_token TEXT,
    server_epoch TEXT
);
",
        );

        let _ = write!(
            schema,
            "\nCREATE TABLE IF NOT EXISTS {} (\n    id TEXT PRIMARY KEY",
            self.name
        );
        for column in &self.columns {
            let _ = write!(schema, ",\n    {}", sqlite_column_ddl(column));
        }
        let _ = write!(
            schema,
            ",\n    {}",
            sqlite_column_ddl(&ColumnDef::new(LIVENESS_COLUMN, ColumnType::Boolean))
        );
        schema.push_str("\n);\n");

        let _ = write!(
            schema,
            "\nCREATE TABLE IF NOT EXISTS {crdt} (
    todo_id TEXT NOT NULL,
    column_name TEXT NOT NULL,
    value TEXT NOT NULL,
    lamport_clock BIGINT NOT NULL CHECK (lamport_clock >= 0),
    device_id TEXT NOT NULL,
    dirty INTEGER NOT NULL DEFAULT 1 CHECK (dirty IN (0, 1)),
    PRIMARY KEY (todo_id, column_name),
    FOREIGN KEY (todo_id) REFERENCES {name}(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS {crdt}_dirty_idx
    ON {crdt}(dirty, todo_id, column_name);
",
            name = self.name,
        );

        // Insert trigger: capture every synchronized column for a new row.
        let _ = write!(
            schema,
            "
CREATE TRIGGER IF NOT EXISTS {crdt}_after_insert
AFTER INSERT ON {name}
WHEN (SELECT applying_remote FROM client_state WHERE singleton = 1) = 0
BEGIN
{guard}
    UPDATE client_state SET lamport_clock = lamport_clock + 1 WHERE singleton = 1;",
            name = self.name,
            guard = clock_overflow_guard(),
        );
        for column in &synchronized {
            schema.push('\n');
            schema.push_str(&capture_statement(&crdt, column));
        }
        schema.push_str("\nEND;\n");

        // One update trigger per synchronized column.
        for column in &synchronized {
            let _ = write!(
                schema,
                "
CREATE TRIGGER IF NOT EXISTS {crdt}_after_{col}_update
AFTER UPDATE OF {col} ON {name}
WHEN OLD.{col} IS NOT NEW.{col}
 AND (SELECT applying_remote FROM client_state WHERE singleton = 1) = 0
BEGIN
{guard}
    UPDATE client_state SET lamport_clock = lamport_clock + 1 WHERE singleton = 1;
{capture}
END;
",
                name = self.name,
                col = column.name,
                guard = clock_overflow_guard(),
                capture = capture_statement(&crdt, column),
            );
        }

        schema
    }

    /// Generates the multi-tenant `PostgreSQL` server schema: a per-tenant
    /// `lamport` clock, the application table, and the `JSONB`-valued CRDT log,
    /// all keyed by `tenant_id` so tenants are isolated and their clocks
    /// advance — and serialize — independently.
    #[must_use]
    pub fn postgres_schema(&self) -> String {
        let crdt = self.crdt_table();

        let mut schema = String::from(
            "
CREATE TABLE IF NOT EXISTS loomabase_state (
    tenant_id TEXT PRIMARY KEY,
    lamport_clock BIGINT NOT NULL CHECK (lamport_clock >= 0)
);
CREATE TABLE IF NOT EXISTS loomabase_cursor_lease (
    tenant_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    crdt_table TEXT NOT NULL,
    max_issued_cursor BIGINT NOT NULL CHECK (max_issued_cursor >= 0),
    cursor_token TEXT NOT NULL DEFAULT gen_random_uuid()::text,
    server_epoch TEXT NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, device_id, crdt_table)
);
CREATE TABLE IF NOT EXISTS loomabase_server_state (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    server_epoch TEXT NOT NULL
);
INSERT INTO loomabase_server_state(singleton, server_epoch)
VALUES (TRUE, gen_random_uuid()::text)
ON CONFLICT(singleton) DO NOTHING;
CREATE TABLE IF NOT EXISTS loomabase_audit_log (
    audit_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    tenant_id TEXT NOT NULL,
    table_name TEXT NOT NULL,
    todo_id TEXT NOT NULL,
    column_name TEXT NOT NULL,
    device_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    reason TEXT NOT NULL,
    incoming_value JSONB NOT NULL,
    incoming_lamport BIGINT NOT NULL CHECK (incoming_lamport >= 0),
    incoming_device_id TEXT NOT NULL,
    current_value JSONB,
    current_lamport BIGINT CHECK (current_lamport IS NULL OR current_lamport >= 0),
    current_device_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX IF NOT EXISTS loomabase_audit_log_tenant_created_idx
    ON loomabase_audit_log(tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS loomabase_audit_log_cell_idx
    ON loomabase_audit_log(tenant_id, table_name, todo_id, column_name, created_at DESC);
CREATE SEQUENCE IF NOT EXISTS loomabase_seq;
",
        );

        let _ = write!(
            schema,
            "\nCREATE TABLE IF NOT EXISTS {} (\n    tenant_id TEXT NOT NULL,\n    id TEXT NOT NULL",
            self.name
        );
        for column in &self.columns {
            let _ = write!(schema, ",\n    {}", postgres_column_ddl(column));
        }
        let _ = write!(
            schema,
            ",\n    {}",
            postgres_column_ddl(&ColumnDef::new(LIVENESS_COLUMN, ColumnType::Boolean))
        );
        schema.push_str(",\n    PRIMARY KEY (tenant_id, id)\n);\n");

        let _ = write!(
            schema,
            "\nCREATE TABLE IF NOT EXISTS {crdt} (
    tenant_id TEXT NOT NULL,
    todo_id TEXT NOT NULL,
    column_name TEXT NOT NULL,
    value JSONB NOT NULL,
    lamport_clock BIGINT NOT NULL CHECK (lamport_clock >= 0),
    device_id TEXT NOT NULL,
    seq BIGINT NOT NULL DEFAULT nextval('loomabase_seq'),
    PRIMARY KEY(tenant_id, todo_id, column_name),
    FOREIGN KEY (tenant_id, todo_id) REFERENCES {name}(tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS {crdt}_clock_idx
    ON {crdt}(tenant_id, lamport_clock, device_id);
CREATE INDEX IF NOT EXISTS {crdt}_seq_idx
    ON {crdt}(tenant_id, seq);
",
            name = self.name,
        );

        schema
    }

    /// Row-Level Security DDL for this contract's tenant-keyed tables. It takes
    /// exclusive table locks, so production deployments should apply it in a
    /// dedicated migration phase. Even a query that forgets its tenant filter
    /// then sees only the current transaction's tenant, and a connection that
    /// never sets the context sees nothing (fail-safe).
    #[must_use]
    pub fn postgres_rls_policies(&self) -> String {
        let crdt = self.crdt_table();
        let mut sql = String::new();
        for table in [
            "loomabase_state",
            "loomabase_cursor_lease",
            "loomabase_audit_log",
            self.name(),
            &crdt,
        ] {
            sql.push_str(&postgres_rls(table));
        }
        sql
    }
}

fn sqlite_column_ddl(column: &ColumnDef) -> String {
    let mut ddl = format!(
        "{} {} NOT NULL DEFAULT {}",
        column.name,
        column.ty.sqlite_type(),
        column.ty.sqlite_default()
    );
    if column.ty == ColumnType::Boolean {
        let _ = write!(ddl, " CHECK ({} IN (0, 1))", column.name);
    }
    ddl
}

fn postgres_column_ddl(column: &ColumnDef) -> String {
    format!(
        "{} {} NOT NULL DEFAULT {}",
        column.name,
        column.ty.postgres_type(),
        column.ty.postgres_default()
    )
}

/// Tenant-isolation Row-Level Security for a tenant-keyed table. `FORCE` keeps
/// the table owner subject to the policy (only superusers bypass it). The
/// `missing_ok` `current_setting` returns NULL when the context is unset, so an
/// unconfigured connection matches no rows.
fn postgres_rls(table: &str) -> String {
    format!(
        "
ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;
ALTER TABLE {table} FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS loomabase_tenant_isolation ON {table};
CREATE POLICY loomabase_tenant_isolation ON {table}
    USING (tenant_id = current_setting('loomabase.tenant_id', true))
    WITH CHECK (tenant_id = current_setting('loomabase.tenant_id', true));
"
    )
}

fn clock_overflow_guard() -> String {
    format!(
        "    SELECT CASE
        WHEN (SELECT lamport_clock FROM client_state WHERE singleton = 1) >= {I64_MAX_LITERAL}
        THEN RAISE(ABORT, 'lamport clock overflow')
    END;"
    )
}

fn capture_statement(crdt_table: &str, column: &ColumnDef) -> String {
    format!(
        "    INSERT INTO {crdt_table}(todo_id, column_name, value, lamport_clock, device_id, dirty)
    SELECT NEW.id, '{col}', {value}, lamport_clock, device_id, 1
    FROM client_state WHERE singleton = 1
    ON CONFLICT(todo_id, column_name) DO UPDATE SET
        value = excluded.value,
        lamport_clock = excluded.lamport_clock,
        device_id = excluded.device_id,
        dirty = 1;",
        col = column.name,
        value = column.ty.sqlite_capture_expr(&column.name),
    )
}

/// A synchronization contract over one or more [`TableDef`]s sharing a single
/// edge database. Each table keeps its own application and CRDT tables and
/// synchronizes independently through the per-table protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contract {
    tables: Vec<TableDef>,
}

impl Contract {
    /// Builds a validated contract.
    ///
    /// # Errors
    /// Returns an error when no table is declared or two tables share a name.
    pub fn new(tables: Vec<TableDef>) -> Result<Self> {
        if tables.is_empty() {
            return Err(SyncError::InvalidPayload(
                "a contract needs at least one table".to_owned(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for table in &tables {
            if !seen.insert(table.name().to_owned()) {
                return Err(SyncError::InvalidPayload(format!(
                    "duplicate table {}",
                    table.name()
                )));
            }
        }
        Ok(Self { tables })
    }

    #[must_use]
    pub fn tables(&self) -> &[TableDef] {
        &self.tables
    }

    /// Generates the `SQLite` schema for every table. The fixed `client_state`
    /// table is emitted by each table's schema with `CREATE TABLE IF NOT EXISTS`,
    /// so it is created once and is a no-op thereafter.
    #[must_use]
    pub fn sqlite_schema(&self) -> String {
        self.tables.iter().map(TableDef::sqlite_schema).collect()
    }

    /// A fingerprint over every table, deterministic and order-independent, for
    /// a multi-table schema handshake.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut fingerprints: Vec<(String, u64)> = self
            .tables
            .iter()
            .map(|table| (table.name().to_owned(), table.fingerprint()))
            .collect();
        fingerprints.sort();
        let mut descriptor = String::from("loomabase-contract-v1");
        for (name, fingerprint) in fingerprints {
            let _ = write!(descriptor, ";{name}={fingerprint:016x}");
        }
        fnv1a_64(descriptor.as_bytes())
    }
}

fn incompatible_type_change(column: &str) -> SyncError {
    SyncError::InvalidPayload(format!(
        "incompatible schema change: column {column} already exists with a different type"
    ))
}

fn reject_removed_columns<'a>(
    existing: &BTreeMap<String, String>,
    expected: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let expected = expected.collect::<std::collections::BTreeSet<_>>();
    if let Some(removed) = existing
        .keys()
        .find(|column| !expected.contains(column.as_str()))
    {
        return Err(SyncError::InvalidPayload(format!(
            "incompatible schema change: existing column {removed} was removed from the contract"
        )));
    }
    Ok(())
}

/// 64-bit FNV-1a: a tiny, dependency-free, deterministic hash. Used only as a
/// schema compatibility fingerprint, not for security.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Validates a developer-supplied SQL identifier. Strict lowercase form keeps
/// generated DDL injection-free and portable between `SQLite` and `PostgreSQL`.
fn validate_sql_identifier(kind: &str, name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_IDENTIFIER_LEN
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(SyncError::InvalidPayload(format!(
            "{kind} {name:?} must be a lowercase [a-z0-9_] SQL identifier of 1..={MAX_IDENTIFIER_LEN} bytes"
        )))
    }
}

/// The canonical `todos` contract: the reference application schema.
#[must_use]
pub fn todos_table() -> TableDef {
    TableDef {
        name: "todos".to_owned(),
        columns: vec![
            ColumnDef::new(TITLE_COLUMN, ColumnType::Text),
            ColumnDef::new(COMPLETED_COLUMN, ColumnType::Boolean),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_and_reserved_identifiers() {
        assert!(TableDef::new("todos; DROP", vec![ColumnDef::new("a", ColumnType::Text)]).is_err());
        assert!(TableDef::new("Todos", vec![ColumnDef::new("a", ColumnType::Text)]).is_err());
        assert!(TableDef::new("todos", vec![]).is_err());
        assert!(TableDef::new("todos", vec![ColumnDef::new("id", ColumnType::Text)]).is_err());
        assert!(
            TableDef::new(
                "todos",
                vec![ColumnDef::new("deleted", ColumnType::Boolean)]
            )
            .is_err()
        );
        assert!(
            TableDef::new(
                "todos",
                vec![
                    ColumnDef::new("a", ColumnType::Text),
                    ColumnDef::new("a", ColumnType::Integer),
                ]
            )
            .is_err()
        );
        assert!(
            TableDef::new(
                "loomabase_state",
                vec![ColumnDef::new("a", ColumnType::Text)]
            )
            .is_err()
        );
    }

    #[test]
    fn synchronized_columns_append_liveness() {
        let table = todos_table();
        assert_eq!(
            table.synchronized_column_names(),
            vec![
                "title".to_owned(),
                "completed".to_owned(),
                "deleted".to_owned()
            ]
        );
        assert_eq!(table.column_type("title"), Some(ColumnType::Text));
        assert_eq!(table.column_type("completed"), Some(ColumnType::Boolean));
        assert_eq!(table.column_type("deleted"), Some(ColumnType::Boolean));
        assert_eq!(table.column_type("missing"), None);
    }

    #[test]
    fn sqlite_schema_covers_tables_triggers_and_liveness() {
        let sql = todos_table().sqlite_schema();
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS todos"));
        assert!(sql.contains("deleted BOOLEAN NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1))"));
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS todos_crdt"));
        assert!(sql.contains("CREATE TRIGGER IF NOT EXISTS todos_crdt_after_insert"));
        assert!(sql.contains("CREATE TRIGGER IF NOT EXISTS todos_crdt_after_title_update"));
        assert!(sql.contains("CREATE TRIGGER IF NOT EXISTS todos_crdt_after_completed_update"));
        assert!(sql.contains("CREATE TRIGGER IF NOT EXISTS todos_crdt_after_deleted_update"));
        // Text captured verbatim, other domains cast to text.
        assert!(sql.contains("SELECT NEW.id, 'title', NEW.title,"));
        assert!(sql.contains("SELECT NEW.id, 'completed', CAST(NEW.completed AS TEXT),"));
    }

    #[test]
    fn generates_custom_table_with_scalar_columns() {
        let notes = TableDef::new(
            "notes",
            vec![
                ColumnDef::new("body", ColumnType::Text),
                ColumnDef::new("priority", ColumnType::Integer),
                ColumnDef::new("pinned", ColumnType::Boolean),
            ],
        )
        .unwrap();

        let sqlite = notes.sqlite_schema();
        assert!(sqlite.contains("CREATE TABLE IF NOT EXISTS notes"));
        assert!(sqlite.contains("priority INTEGER NOT NULL DEFAULT 0"));
        assert!(sqlite.contains("CREATE TRIGGER IF NOT EXISTS notes_crdt_after_priority_update"));
        assert!(sqlite.contains("SELECT NEW.id, 'priority', CAST(NEW.priority AS TEXT),"));

        let postgres = notes.postgres_schema();
        assert!(postgres.contains("priority BIGINT NOT NULL DEFAULT 0"));
        assert!(postgres.contains("tenant_id TEXT NOT NULL"));
        assert!(postgres.contains("PRIMARY KEY (tenant_id, id)"));
        assert!(postgres.contains("REFERENCES notes(tenant_id, id) ON DELETE CASCADE"));
    }

    #[test]
    fn fingerprint_is_stable_and_distinguishes_contracts() {
        let base = todos_table();
        assert_eq!(base.fingerprint(), todos_table().fingerprint());

        let renamed_table = TableDef::new(
            "tasks",
            vec![
                ColumnDef::new("title", ColumnType::Text),
                ColumnDef::new("completed", ColumnType::Boolean),
            ],
        )
        .unwrap();
        let retyped_column = TableDef::new(
            "todos",
            vec![
                ColumnDef::new("title", ColumnType::Text),
                ColumnDef::new("completed", ColumnType::Integer),
            ],
        )
        .unwrap();
        let extra_column = TableDef::new(
            "todos",
            vec![
                ColumnDef::new("title", ColumnType::Text),
                ColumnDef::new("completed", ColumnType::Boolean),
                ColumnDef::new("notes", ColumnType::Text),
            ],
        )
        .unwrap();

        assert_ne!(base.fingerprint(), renamed_table.fingerprint());
        assert_ne!(base.fingerprint(), retyped_column.fingerprint());
        assert_ne!(base.fingerprint(), extra_column.fingerprint());
    }

    #[test]
    fn sqlite_migration_adds_missing_columns_and_rejects_type_changes() {
        let table = TableDef::new(
            "notes",
            vec![
                ColumnDef::new("body", ColumnType::Text),
                ColumnDef::new("priority", ColumnType::Integer),
            ],
        )
        .unwrap();

        // A table that predates the `priority` column gains it additively.
        let mut existing = BTreeMap::from([
            ("body".to_owned(), "TEXT".to_owned()),
            ("deleted".to_owned(), "BOOLEAN".to_owned()),
        ]);
        let statements = table.sqlite_migration_sql(&existing).unwrap();
        assert!(
            statements
                .iter()
                .any(|s| s.contains("ALTER TABLE notes ADD COLUMN priority INTEGER"))
        );
        assert!(
            statements
                .iter()
                .any(|s| s.contains("DROP TRIGGER IF EXISTS notes_crdt_after_insert"))
        );

        // A fully up-to-date table needs no migration.
        existing.insert("priority".to_owned(), "INTEGER".to_owned());
        assert!(table.sqlite_migration_sql(&existing).unwrap().is_empty());

        // A retyped column is a destructive change and is rejected.
        existing.insert("priority".to_owned(), "TEXT".to_owned());
        assert!(table.sqlite_migration_sql(&existing).is_err());
    }

    #[test]
    fn postgres_crdt_migration_backfills_and_enforces_sequence_default() {
        let table = todos_table();
        let existing = BTreeMap::from([
            ("tenant_id".to_owned(), "text".to_owned()),
            ("todo_id".to_owned(), "text".to_owned()),
            ("column_name".to_owned(), "text".to_owned()),
        ]);
        let migration = table.postgres_crdt_migration_sql(&existing).unwrap();
        assert!(
            migration
                .iter()
                .any(|sql| sql.contains("SET seq = nextval('loomabase_seq')"))
        );
        assert!(
            migration
                .iter()
                .any(|sql| sql.contains("SET DEFAULT nextval('loomabase_seq')"))
        );

        let invalid = BTreeMap::from([("seq".to_owned(), "text".to_owned())]);
        assert!(table.postgres_crdt_migration_sql(&invalid).is_err());
    }
}
