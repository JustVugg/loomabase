//! Authoritative partial-replica membership protocol and safe query planning.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crdt::{CrdtValue, MAX_PAYLOAD_ROWS, SyncPayload, validate_column, validate_identifier};
use crate::error::{Result, SyncError};
use crate::schema::TableDef;

pub const MAX_KNOWN_MEMBERS: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ReplicaPredicate {
    IdEquals(String),
    IdPrefix(String),
    ColumnEquals { column: String, value: CrdtValue },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplicaInterest {
    pub predicates: Vec<ReplicaPredicate>,
    pub limit: u32,
}

/// Client request for one authoritative partial-replica scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartialReplicaRequest {
    pub scope_id: String,
    /// Monotonic client-issued revision. Responses with an older revision are
    /// rejected locally, preventing out-of-order transports from reverting
    /// authoritative membership.
    pub scope_version: u64,
    pub interest: ReplicaInterest,
    pub known_member_ids: Vec<String>,
    pub sync: SyncPayload,
}

/// Authoritative scope snapshot. Rows absent from `member_ids` are evicted
/// locally, never converted into global tombstones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartialReplicaResponse {
    pub scope_id: String,
    pub scope_version: u64,
    pub member_ids: Vec<String>,
    pub evicted_row_ids: Vec<String>,
    pub sync: SyncPayload,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueryPlan {
    pub sql: String,
    pub parameters: Vec<CrdtValue>,
}

impl ReplicaInterest {
    pub fn validate(&self, table: &TableDef) -> Result<()> {
        if self.limit == 0 || self.limit as usize > MAX_PAYLOAD_ROWS {
            return Err(SyncError::InvalidPayload(format!(
                "replica interest limit must be between 1 and {MAX_PAYLOAD_ROWS}"
            )));
        }
        if self.predicates.len() > 32 {
            return Err(SyncError::InvalidPayload(
                "replica interest exceeds 32 predicates".to_owned(),
            ));
        }
        for predicate in &self.predicates {
            match predicate {
                ReplicaPredicate::IdEquals(value) | ReplicaPredicate::IdPrefix(value) => {
                    if value.is_empty() || value.len() > 255 {
                        return Err(SyncError::InvalidPayload(
                            "replica ID predicate must contain 1 to 255 bytes".to_owned(),
                        ));
                    }
                }
                ReplicaPredicate::ColumnEquals { column, value } => {
                    validate_column(table, column, value)?;
                }
            }
        }
        Ok(())
    }

    pub fn postgres_plan(&self, table: &TableDef, tenant_id: &str) -> Result<QueryPlan> {
        self.validate(table)?;
        let mut clauses = vec!["tenant_id = $1".to_owned(), "deleted = FALSE".to_owned()];
        let mut parameters = vec![CrdtValue::Text(tenant_id.to_owned())];
        for predicate in &self.predicates {
            let index = parameters.len() + 1;
            match predicate {
                ReplicaPredicate::IdEquals(value) => {
                    clauses.push(format!("id = ${index}"));
                    parameters.push(CrdtValue::Text(value.clone()));
                }
                ReplicaPredicate::IdPrefix(value) => {
                    clauses.push(format!("starts_with(id, ${index})"));
                    parameters.push(CrdtValue::Text(value.clone()));
                }
                ReplicaPredicate::ColumnEquals { column, value } => {
                    clauses.push(format!("{column} = ${index}"));
                    parameters.push(value.clone());
                }
            }
        }
        Ok(QueryPlan {
            sql: format!(
                "SELECT * FROM {} WHERE {} ORDER BY id LIMIT {}",
                table.name(),
                clauses.join(" AND "),
                self.limit
            ),
            parameters,
        })
    }

    #[must_use]
    pub fn matches(&self, row_id: &str, values: &BTreeMap<String, CrdtValue>) -> bool {
        self.predicates.iter().all(|predicate| match predicate {
            ReplicaPredicate::IdEquals(expected) => row_id == expected,
            ReplicaPredicate::IdPrefix(prefix) => row_id.starts_with(prefix),
            ReplicaPredicate::ColumnEquals { column, value } => values.get(column) == Some(value),
        })
    }
}

impl PartialReplicaRequest {
    pub fn validate(&self, table: &TableDef, authenticated_device_id: &str) -> Result<()> {
        validate_identifier("scope_id", &self.scope_id)?;
        validate_scope_version(self.scope_version)?;
        self.interest.validate(table)?;
        self.sync
            .validate_client_request(authenticated_device_id, table)?;
        validate_member_ids(&self.known_member_ids)?;
        Ok(())
    }
}

impl PartialReplicaResponse {
    pub fn validate(&self, table: &TableDef) -> Result<()> {
        validate_identifier("scope_id", &self.scope_id)?;
        validate_scope_version(self.scope_version)?;
        self.sync.validate_server_response(table)?;
        validate_member_ids(&self.member_ids)?;
        validate_member_ids(&self.evicted_row_ids)?;
        if self
            .member_ids
            .iter()
            .any(|member| self.evicted_row_ids.binary_search(member).is_ok())
        {
            return Err(SyncError::InvalidPayload(
                "a row cannot be both a scope member and evicted".to_owned(),
            ));
        }
        let snapshot_ids = self
            .sync
            .changes
            .iter()
            .map(|row| row.todo_id.as_str())
            .collect::<Vec<_>>();
        if snapshot_ids
            .iter()
            .copied()
            .ne(self.member_ids.iter().map(String::as_str))
        {
            return Err(SyncError::InvalidPayload(
                "partial replica snapshot rows must exactly match member_ids".to_owned(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_member_ids(ids: &[String]) -> Result<()> {
    if ids.len() > MAX_KNOWN_MEMBERS {
        return Err(SyncError::InvalidPayload(format!(
            "partial replica exceeds the {MAX_KNOWN_MEMBERS} member limit"
        )));
    }
    let mut previous: Option<&str> = None;
    for id in ids {
        validate_identifier("partial replica member ID", id)?;
        if previous.is_some_and(|value| value >= id.as_str()) {
            return Err(SyncError::InvalidPayload(
                "partial replica member IDs must be unique and sorted".to_owned(),
            ));
        }
        previous = Some(id);
    }
    Ok(())
}

fn validate_scope_version(version: u64) -> Result<()> {
    if version == 0 || i64::try_from(version).is_err() {
        return Err(SyncError::InvalidPayload(
            "partial replica scope_version must be between 1 and i64::MAX".to_owned(),
        ));
    }
    Ok(())
}
