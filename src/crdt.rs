use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Result, SyncError};
use crate::replica::{PartialReplicaRequest, PartialReplicaResponse};
use crate::schema::{ColumnType, TableDef};

pub const TITLE_COLUMN: &str = "title";
pub const COMPLETED_COLUMN: &str = "completed";
/// Per-row liveness register. `true` is a tombstone; `false` is a live row.
/// Creation, deletion, and restoration all write this LWW register, so the most
/// recent `(lamport_clock, device_id)` decides whether the row exists.
pub const DELETED_COLUMN: &str = "deleted";
pub const SERVER_DEVICE_ID: &str = "loomabase-server";
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u16 = 3;
pub const PROTOCOL_VERSION: u16 = 4;
pub const MAX_PAYLOAD_ROWS: usize = 10_000;
pub const MAX_PAYLOAD_CELLS: usize = 20_000;
pub const MAX_RESPONSE_CELLS: usize = 1_000;
pub const MAX_RESPONSE_BYTES: usize = 4 * 1_048_576;
pub const MAX_TEXT_BYTES: usize = 1_048_576;
pub const MAX_STORABLE_LAMPORT: u64 = i64::MAX as u64;
pub const MAX_CLOCK_ADVANCE_PER_SYNC: u64 = 1_000_000;

/// SQL values transported without implicit or ambiguous conversions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CrdtValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Boolean(bool),
    Blob(Vec<u8>),
}

/// Lexicographic `(lamport_clock, device_id)` ordering makes LWW total and deterministic.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ColumnMetadata {
    pub lamport_clock: u64,
    pub device_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrdtColumn {
    pub value: CrdtValue,
    pub metadata: ColumnMetadata,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowChange {
    pub todo_id: String,
    pub columns: BTreeMap<String, CrdtColumn>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncPayload {
    pub protocol_version: u16,
    pub schema_fingerprint: u64,
    pub source_device_id: String,
    pub source_lamport: u64,
    pub changes: Vec<RowChange>,
    /// Anti-entropy change-feed cursor. In a client request it is the highest
    /// server `seq` the client has applied; in a server response it is the new
    /// high-water mark to send back next time. `0` requests a full sync.
    pub cursor: i64,
    /// Server responses set this when more change-feed pages remain. Client
    /// requests must always set it to `false`.
    #[serde(default)]
    pub has_more: bool,
    /// Server responses set this when the supplied cursor was not valid for the
    /// current server/tenant/device and the page starts a full repair.
    #[serde(default)]
    pub cursor_reset: bool,
    /// Opaque server-issued capability bound to the cursor, tenant, device,
    /// table, and server epoch. Version-3 peers omit it and use lease-only
    /// validation during rolling upgrades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_token: Option<String>,
    /// Identifies the server data epoch. Restores or failovers can rotate it so
    /// clients automatically discard cursors from a different history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_epoch: Option<String>,
}

impl SyncPayload {
    #[must_use]
    pub fn empty(
        source_device_id: impl Into<String>,
        source_lamport: u64,
        table: &TableDef,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            schema_fingerprint: table.fingerprint(),
            source_device_id: source_device_id.into(),
            source_lamport,
            changes: Vec::new(),
            cursor: 0,
            has_more: false,
            cursor_reset: false,
            cursor_token: None,
            server_epoch: None,
        }
    }

    pub fn validate(&self, table: &TableDef) -> Result<()> {
        if !(MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&self.protocol_version) {
            return Err(SyncError::InvalidPayload(format!(
                "unsupported protocol version {}; supported range is {MIN_SUPPORTED_PROTOCOL_VERSION}..={PROTOCOL_VERSION}",
                self.protocol_version
            )));
        }
        let local_fingerprint = table.fingerprint();
        if self.schema_fingerprint != local_fingerprint {
            return Err(SyncError::SchemaMismatch {
                expected: local_fingerprint,
                actual: self.schema_fingerprint,
            });
        }
        validate_identifier("source_device_id", &self.source_device_id)?;
        validate_clock(self.source_lamport)?;
        if self.cursor < 0 {
            return Err(SyncError::InvalidPayload(
                "cursor cannot be negative".to_owned(),
            ));
        }
        for (name, value) in [
            ("cursor_token", self.cursor_token.as_deref()),
            ("server_epoch", self.server_epoch.as_deref()),
        ] {
            if let Some(value) = value {
                validate_identifier(name, value)?;
            }
        }
        if self.changes.len() > MAX_PAYLOAD_ROWS {
            return Err(SyncError::InvalidPayload(format!(
                "payload exceeds the {MAX_PAYLOAD_ROWS} changed-row limit"
            )));
        }

        let mut changed_cells = BTreeSet::new();
        let mut changed_rows = BTreeSet::new();
        let mut cell_count = 0_usize;
        for row in &self.changes {
            validate_identifier("todo_id", &row.todo_id)?;
            if !changed_rows.insert(row.todo_id.clone()) {
                return Err(SyncError::InvalidPayload(
                    "payload contains a duplicate row".to_owned(),
                ));
            }
            if row.columns.is_empty() {
                return Err(SyncError::InvalidPayload(format!(
                    "row {} does not contain columns",
                    row.todo_id
                )));
            }

            for (column_name, column) in &row.columns {
                cell_count += 1;
                if cell_count > MAX_PAYLOAD_CELLS {
                    return Err(SyncError::InvalidPayload(format!(
                        "payload exceeds the {MAX_PAYLOAD_CELLS} changed-cell limit"
                    )));
                }
                validate_column(table, column_name, &column.value)?;
                validate_identifier("metadata.device_id", &column.metadata.device_id)?;
                validate_clock(column.metadata.lamport_clock)?;
                let key = (row.todo_id.clone(), column_name.clone());
                if !changed_cells.insert(key) {
                    return Err(SyncError::InvalidPayload(
                        "payload contains a duplicate cell".to_owned(),
                    ));
                }
            }
        }

        Ok(())
    }

    pub fn validate_client_request(
        &self,
        authenticated_device_id: &str,
        table: &TableDef,
    ) -> Result<()> {
        self.validate(table)?;
        validate_identifier("authenticated_device_id", authenticated_device_id)?;

        if authenticated_device_id == SERVER_DEVICE_ID {
            return Err(SyncError::InvalidPayload(
                "the reserved server device ID cannot authenticate as a client".to_owned(),
            ));
        }
        if self.source_device_id != authenticated_device_id {
            return Err(SyncError::InvalidPayload(
                "source_device_id does not match the authenticated device".to_owned(),
            ));
        }
        if self.has_more || self.cursor_reset {
            return Err(SyncError::InvalidPayload(
                "a client request cannot set response-only cursor flags".to_owned(),
            ));
        }

        for row in &self.changes {
            for column in row.columns.values() {
                if column.metadata.device_id != authenticated_device_id {
                    return Err(SyncError::InvalidPayload(
                        "a client cannot attribute a change to another device".to_owned(),
                    ));
                }
                if column.metadata.lamport_clock > self.source_lamport {
                    return Err(SyncError::InvalidPayload(
                        "a column clock cannot exceed the source clock".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn validate_server_response(&self, table: &TableDef) -> Result<()> {
        self.validate(table)?;
        if self.source_device_id != SERVER_DEVICE_ID {
            return Err(SyncError::InvalidPayload(
                "remote payload is not attributed to the Loomabase server".to_owned(),
            ));
        }
        if self
            .changes
            .iter()
            .flat_map(|row| row.columns.values())
            .any(|column| column.metadata.lamport_clock > self.source_lamport)
        {
            return Err(SyncError::InvalidPayload(
                "a response column clock cannot exceed the server source clock".to_owned(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn max_observed_clock(&self) -> u64 {
        self.changes
            .iter()
            .flat_map(|row| row.columns.values())
            .map(|column| column.metadata.lamport_clock)
            .fold(self.source_lamport, u64::max)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeDecision {
    AcceptIncoming,
    KeepCurrent,
    Equal,
}

#[must_use]
pub fn decide_lww(current: &ColumnMetadata, incoming: &ColumnMetadata) -> MergeDecision {
    match incoming.cmp(current) {
        Ordering::Greater => MergeDecision::AcceptIncoming,
        Ordering::Less => MergeDecision::KeepCurrent,
        Ordering::Equal => MergeDecision::Equal,
    }
}

/// In-memory reference implementation of the merge used by the `PostgreSQL` adapter.
/// It is useful for deterministic tests and protocol validation without I/O.
#[derive(Clone, Debug, PartialEq)]
pub struct CrdtState {
    pub global_lamport: u64,
    pub cells: BTreeMap<(String, String), CrdtColumn>,
    table: TableDef,
    /// Monotonic write counter that mirrors the `PostgreSQL` `seq`, driving the
    /// anti-entropy cursor.
    seq: u64,
    cell_seq: BTreeMap<(String, String), u64>,
    issued_cursors: BTreeMap<String, i64>,
}

impl Default for CrdtState {
    fn default() -> Self {
        Self::new(crate::schema::todos_table())
    }
}

impl CrdtState {
    /// Creates an empty reference server bound to a synchronization contract.
    #[must_use]
    pub fn new(table: TableDef) -> Self {
        Self {
            global_lamport: 0,
            cells: BTreeMap::new(),
            table,
            seq: 0,
            cell_seq: BTreeMap::new(),
            issued_cursors: BTreeMap::new(),
        }
    }

    pub fn merge(
        &mut self,
        client_payload: SyncPayload,
        authenticated_device_id: &str,
    ) -> Result<SyncPayload> {
        let mut working = self.clone();
        let response = working.merge_inner(client_payload, authenticated_device_id)?;
        *self = working;
        Ok(response)
    }

    /// Deterministic reference implementation of authoritative partial-replica
    /// membership. It mirrors the `PostgreSQL` adapter and is used for protocol
    /// model tests without external infrastructure.
    pub fn merge_partial(
        &mut self,
        request: PartialReplicaRequest,
        authenticated_device_id: &str,
    ) -> Result<PartialReplicaResponse> {
        let mut working = self.clone();
        let response = working.merge_partial_inner(request, authenticated_device_id)?;
        *self = working;
        Ok(response)
    }

    fn merge_partial_inner(
        &mut self,
        request: PartialReplicaRequest,
        authenticated_device_id: &str,
    ) -> Result<PartialReplicaResponse> {
        request.validate(&self.table, authenticated_device_id)?;
        let scope_id = request.scope_id.clone();
        let scope_version = request.scope_version;
        let interest = request.interest.clone();
        let known_member_ids = request.known_member_ids.clone();
        let mut sync = self.merge(request.sync, authenticated_device_id)?;

        let mut values_by_row: BTreeMap<String, BTreeMap<String, CrdtValue>> = BTreeMap::new();
        for ((row_id, column_name), column) in &self.cells {
            values_by_row
                .entry(row_id.clone())
                .or_default()
                .insert(column_name.clone(), column.value.clone());
        }
        let member_ids = values_by_row
            .iter()
            .filter(|(_, values)| values.get(DELETED_COLUMN) != Some(&CrdtValue::Boolean(true)))
            .map(|(row_id, _)| row_id.clone())
            .filter(|row_id| interest.matches(row_id, &values_by_row[row_id]))
            .collect::<Vec<_>>();
        if member_ids.len() > interest.limit as usize {
            return Err(SyncError::InvalidPayload(format!(
                "partial replica scope exceeds its declared limit of {} rows",
                interest.limit
            )));
        }
        let mut snapshot_by_row: BTreeMap<String, BTreeMap<String, CrdtColumn>> = BTreeMap::new();
        let mut snapshot_bytes = 0_usize;
        for ((row_id, column_name), column) in &self.cells {
            if member_ids.binary_search(row_id).is_ok() {
                snapshot_bytes +=
                    serde_json::to_vec(column)?.len() + row_id.len() + column_name.len();
                if snapshot_bytes > MAX_RESPONSE_BYTES {
                    return Err(SyncError::InvalidPayload(format!(
                        "partial replica snapshot exceeds the {MAX_RESPONSE_BYTES}-byte limit"
                    )));
                }
                snapshot_by_row
                    .entry(row_id.clone())
                    .or_default()
                    .insert(column_name.clone(), column.clone());
            }
        }
        let evicted_row_ids = known_member_ids
            .iter()
            .filter(|row_id| member_ids.binary_search(row_id).is_err())
            .cloned()
            .collect();
        sync.changes = snapshot_by_row
            .into_iter()
            .map(|(todo_id, columns)| RowChange { todo_id, columns })
            .collect();
        sync.has_more = false;
        let response = PartialReplicaResponse {
            scope_id,
            scope_version,
            member_ids,
            evicted_row_ids,
            sync,
        };
        response.validate(&self.table)?;
        Ok(response)
    }

    fn merge_inner(
        &mut self,
        client_payload: SyncPayload,
        authenticated_device_id: &str,
    ) -> Result<SyncPayload> {
        client_payload.validate_client_request(authenticated_device_id, &self.table)?;
        let response_protocol_version = client_payload.protocol_version;
        let observed_clock = client_payload.max_observed_clock();
        validate_clock_advance(self.global_lamport, observed_clock)?;

        for row in client_payload.changes {
            for (column_name, incoming) in row.columns {
                let key = (row.todo_id.clone(), column_name);
                let apply = match self.cells.get(&key) {
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
                if apply {
                    self.seq += 1;
                    self.cell_seq.insert(key.clone(), self.seq);
                    self.cells.insert(key, incoming);
                }
            }
        }

        self.global_lamport = self
            .global_lamport
            .max(observed_clock)
            .checked_add(1)
            .ok_or(SyncError::ClockOverflow)?;

        // A cursor ahead of this reference server can come from a restored or
        // different server. Resetting it is fail-safe: the client receives a
        // bounded full repair rather than silently skipping data.
        let server_seq = i64::try_from(self.seq).unwrap_or(i64::MAX);
        let max_issued_cursor = self
            .issued_cursors
            .get(authenticated_device_id)
            .copied()
            .unwrap_or(0);
        let cursor_valid = client_payload.cursor == 0
            || (client_payload.cursor <= server_seq && client_payload.cursor <= max_issued_cursor);
        let cursor_reset = client_payload.cursor != 0 && !cursor_valid;
        let effective_cursor = if cursor_valid {
            client_payload.cursor
        } else {
            0
        };

        // Incremental, bounded change feed mirroring the PostgreSQL adapter.
        let mut candidates = self
            .cells
            .iter()
            .filter_map(|(key, column)| {
                let seq = self.cell_seq.get(key).copied().unwrap_or(0);
                (i64::try_from(seq).unwrap_or(i64::MAX) > effective_cursor)
                    .then_some((seq, key, column))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(seq, _, _)| *seq);

        let mut response_rows: BTreeMap<String, BTreeMap<String, CrdtColumn>> = BTreeMap::new();
        let mut response_bytes = 0_usize;
        let mut next_cursor = effective_cursor;
        let mut included = 0_usize;
        for (seq, (todo_id, column_name), server_column) in &candidates {
            let cell_bytes =
                serde_json::to_vec(server_column)?.len() + todo_id.len() + column_name.len();
            if included >= MAX_RESPONSE_CELLS
                || (included > 0 && response_bytes + cell_bytes > MAX_RESPONSE_BYTES)
            {
                break;
            }
            response_bytes += cell_bytes;
            included += 1;
            next_cursor = i64::try_from(*seq).unwrap_or(i64::MAX);
            response_rows
                .entry(todo_id.clone())
                .or_default()
                .insert(column_name.clone(), (*server_column).clone());
        }
        let has_more = included < candidates.len();
        if !has_more {
            next_cursor = server_seq;
        }
        self.issued_cursors
            .entry(authenticated_device_id.to_owned())
            .and_modify(|cursor| *cursor = (*cursor).max(next_cursor))
            .or_insert(next_cursor);

        Ok(SyncPayload {
            protocol_version: response_protocol_version,
            schema_fingerprint: self.table.fingerprint(),
            source_device_id: SERVER_DEVICE_ID.to_owned(),
            source_lamport: self.global_lamport,
            changes: response_rows
                .into_iter()
                .map(|(todo_id, columns)| RowChange { todo_id, columns })
                .collect(),
            cursor: next_cursor,
            has_more,
            cursor_reset,
            cursor_token: None,
            server_epoch: None,
        })
    }
}

pub fn validate_column(table: &TableDef, column_name: &str, value: &CrdtValue) -> Result<()> {
    let Some(ty) = table.column_type(column_name) else {
        return Err(SyncError::InvalidPayload(format!(
            "column is not synchronizable: {column_name}"
        )));
    };
    match (ty, value) {
        (ColumnType::Text, CrdtValue::Text(text)) if text.len() <= MAX_TEXT_BYTES => Ok(()),
        (ColumnType::Text, CrdtValue::Text(_)) => Err(SyncError::InvalidPayload(format!(
            "text column {column_name} exceeds the {MAX_TEXT_BYTES}-byte limit"
        ))),
        (ColumnType::Integer, CrdtValue::Integer(_))
        | (ColumnType::Boolean, CrdtValue::Boolean(_)) => Ok(()),
        (ColumnType::Real, CrdtValue::Real(real)) if real.is_finite() => Ok(()),
        (ColumnType::Real, CrdtValue::Real(_)) => Err(SyncError::InvalidPayload(format!(
            "real column {column_name} must be finite"
        ))),
        _ => Err(SyncError::InvalidPayload(format!(
            "invalid value type for column {column_name}"
        ))),
    }
}

pub fn validate_column_name(table: &TableDef, column_name: &str) -> Result<()> {
    if table.column_type(column_name).is_some() {
        Ok(())
    } else {
        Err(SyncError::InvalidPayload(format!(
            "column is not synchronizable: {column_name}"
        )))
    }
}

pub fn validate_identifier(field: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 255 || value.chars().any(char::is_control) {
        return Err(SyncError::InvalidPayload(format!(
            "{field} must contain between 1 and 255 non-control characters"
        )));
    }
    Ok(())
}

pub fn validate_clock(clock: u64) -> Result<()> {
    if clock <= MAX_STORABLE_LAMPORT {
        Ok(())
    } else {
        Err(SyncError::ClockOverflow)
    }
}

pub fn validate_clock_advance(current: u64, observed: u64) -> Result<()> {
    if observed.saturating_sub(current) <= MAX_CLOCK_ADVANCE_PER_SYNC {
        Ok(())
    } else {
        Err(SyncError::InvalidPayload(format!(
            "Lamport clock advances by more than the {MAX_CLOCK_ADVANCE_PER_SYNC} per-sync limit"
        )))
    }
}
