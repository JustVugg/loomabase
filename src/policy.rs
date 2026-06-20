//! Authorization, business validation, rejection, and audit primitives for sync.
//!
//! Loomabase verifies transport identity and CRDT correctness, but application
//! ownership remains domain-specific. These hooks let an API enforce field,
//! row, and tenant policy before a valid CRDT cell is allowed to participate in
//! the LWW merge.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, SyncRejectionKind, validate_column_name,
    validate_identifier, validate_rejection_reason,
};
use crate::error::Result;
use crate::schema::TableDef;

#[derive(Clone, Debug)]
pub struct SyncContext<'a> {
    pub tenant_id: &'a str,
    pub authenticated_device_id: &'a str,
    pub table: &'a TableDef,
}

#[derive(Clone, Debug)]
pub struct CellMutation<'a> {
    pub todo_id: &'a str,
    pub column_name: &'a str,
    pub value: &'a CrdtValue,
    pub metadata: &'a ColumnMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyDecision {
    Allow,
    Deny {
        kind: SyncRejectionKind,
        reason: String,
    },
}

impl PolicyDecision {
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow
    }

    pub fn deny(kind: SyncRejectionKind, reason: impl Into<String>) -> Result<Self> {
        let reason = reason.into();
        validate_rejection_reason(&reason)?;
        Ok(Self::Deny { kind, reason })
    }
}

pub trait SyncAuthorizer: Send + Sync + 'static {
    fn authorize_cell(
        &self,
        context: &SyncContext<'_>,
        mutation: &CellMutation<'_>,
        current: Option<&CrdtColumn>,
    ) -> Result<PolicyDecision>;
}

pub trait SyncValidator: Send + Sync + 'static {
    fn validate_cell(
        &self,
        context: &SyncContext<'_>,
        mutation: &CellMutation<'_>,
        current: Option<&CrdtColumn>,
    ) -> Result<PolicyDecision>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllAuthorizer;

impl SyncAuthorizer for AllowAllAuthorizer {
    fn authorize_cell(
        &self,
        _context: &SyncContext<'_>,
        _mutation: &CellMutation<'_>,
        _current: Option<&CrdtColumn>,
    ) -> Result<PolicyDecision> {
        Ok(PolicyDecision::Allow)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopValidator;

impl SyncValidator for NoopValidator {
    fn validate_cell(
        &self,
        _context: &SyncContext<'_>,
        _mutation: &CellMutation<'_>,
        _current: Option<&CrdtColumn>,
    ) -> Result<PolicyDecision> {
        Ok(PolicyDecision::Allow)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnAllowListAuthorizer {
    writable_columns: BTreeSet<String>,
}

impl ColumnAllowListAuthorizer {
    pub fn new<I, S>(table: &TableDef, writable_columns: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut columns = BTreeSet::new();
        for column in writable_columns {
            let column = column.into();
            validate_column_name(table, &column)?;
            columns.insert(column);
        }
        Ok(Self {
            writable_columns: columns,
        })
    }
}

impl SyncAuthorizer for ColumnAllowListAuthorizer {
    fn authorize_cell(
        &self,
        _context: &SyncContext<'_>,
        mutation: &CellMutation<'_>,
        _current: Option<&CrdtColumn>,
    ) -> Result<PolicyDecision> {
        if self.writable_columns.contains(mutation.column_name) {
            return Ok(PolicyDecision::Allow);
        }
        PolicyDecision::deny(
            SyncRejectionKind::AuthorizationDenied,
            format!(
                "writes to column {} are not authorized",
                mutation.column_name
            ),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaxTextLengthValidator {
    max_bytes: usize,
    columns: Option<BTreeSet<String>>,
}

impl MaxTextLengthValidator {
    #[must_use]
    pub const fn all_text_columns(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            columns: None,
        }
    }

    pub fn columns<I, S>(table: &TableDef, max_bytes: usize, columns: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut validated = BTreeSet::new();
        for column in columns {
            let column = column.into();
            validate_column_name(table, &column)?;
            validated.insert(column);
        }
        Ok(Self {
            max_bytes,
            columns: Some(validated),
        })
    }

    fn applies_to(&self, column_name: &str) -> bool {
        self.columns
            .as_ref()
            .is_none_or(|columns| columns.contains(column_name))
    }
}

impl SyncValidator for MaxTextLengthValidator {
    fn validate_cell(
        &self,
        _context: &SyncContext<'_>,
        mutation: &CellMutation<'_>,
        _current: Option<&CrdtColumn>,
    ) -> Result<PolicyDecision> {
        let CrdtValue::Text(text) = mutation.value else {
            return Ok(PolicyDecision::Allow);
        };
        if !self.applies_to(mutation.column_name) || text.len() <= self.max_bytes {
            return Ok(PolicyDecision::Allow);
        }
        PolicyDecision::deny(
            SyncRejectionKind::ValidationFailed,
            format!(
                "text value for column {} exceeds {} bytes",
                mutation.column_name, self.max_bytes
            ),
        )
    }
}

#[derive(Clone)]
pub struct CompositeValidator {
    validators: Vec<Arc<dyn SyncValidator>>,
}

impl CompositeValidator {
    #[must_use]
    pub fn new(validators: Vec<Arc<dyn SyncValidator>>) -> Self {
        Self { validators }
    }
}

impl SyncValidator for CompositeValidator {
    fn validate_cell(
        &self,
        context: &SyncContext<'_>,
        mutation: &CellMutation<'_>,
        current: Option<&CrdtColumn>,
    ) -> Result<PolicyDecision> {
        for validator in &self.validators {
            let decision = validator.validate_cell(context, mutation, current)?;
            if matches!(decision, PolicyDecision::Deny { .. }) {
                return Ok(decision);
            }
        }
        Ok(PolicyDecision::Allow)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AuditMode {
    Disabled,
    #[default]
    Database,
}

#[derive(Clone)]
pub struct SyncSecurity {
    authorizer: Arc<dyn SyncAuthorizer>,
    validator: Arc<dyn SyncValidator>,
    audit_mode: AuditMode,
}

impl Default for SyncSecurity {
    fn default() -> Self {
        Self {
            authorizer: Arc::new(AllowAllAuthorizer),
            validator: Arc::new(NoopValidator),
            audit_mode: AuditMode::default(),
        }
    }
}

impl SyncSecurity {
    #[must_use]
    pub fn new(
        authorizer: Arc<dyn SyncAuthorizer>,
        validator: Arc<dyn SyncValidator>,
        audit_mode: AuditMode,
    ) -> Self {
        Self {
            authorizer,
            validator,
            audit_mode,
        }
    }

    #[must_use]
    pub fn without_audit(
        authorizer: Arc<dyn SyncAuthorizer>,
        validator: Arc<dyn SyncValidator>,
    ) -> Self {
        Self {
            authorizer,
            validator,
            audit_mode: AuditMode::Disabled,
        }
    }

    #[must_use]
    pub fn authorizer(&self) -> &dyn SyncAuthorizer {
        self.authorizer.as_ref()
    }

    #[must_use]
    pub fn validator(&self) -> &dyn SyncValidator {
        self.validator.as_ref()
    }

    #[must_use]
    pub const fn audit_mode(&self) -> AuditMode {
        self.audit_mode
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAuditOutcome {
    Accepted,
    KeptCurrent,
    Idempotent,
    RejectedAuthorization,
    RejectedValidation,
}

impl SyncAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::KeptCurrent => "kept_current",
            Self::Idempotent => "idempotent",
            Self::RejectedAuthorization => "rejected_authorization",
            Self::RejectedValidation => "rejected_validation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncAuditEvent {
    pub table_name: String,
    pub todo_id: String,
    pub column_name: String,
    pub outcome: SyncAuditOutcome,
    pub reason: String,
    pub incoming_value: CrdtValue,
    pub incoming_metadata: ColumnMetadata,
    pub current_value: Option<CrdtValue>,
    pub current_metadata: Option<ColumnMetadata>,
}

impl SyncAuditEvent {
    pub fn new(
        context: &SyncContext<'_>,
        mutation: &CellMutation<'_>,
        current: Option<&CrdtColumn>,
        outcome: SyncAuditOutcome,
        reason: impl Into<String>,
    ) -> Result<Self> {
        let reason = reason.into();
        validate_identifier("audit.table_name", context.table.name())?;
        validate_identifier("audit.todo_id", mutation.todo_id)?;
        validate_identifier("audit.column_name", mutation.column_name)?;
        validate_identifier("audit.incoming_device_id", &mutation.metadata.device_id)?;
        validate_rejection_reason(&reason)?;
        Ok(Self {
            table_name: context.table.name().to_owned(),
            todo_id: mutation.todo_id.to_owned(),
            column_name: mutation.column_name.to_owned(),
            outcome,
            reason,
            incoming_value: mutation.value.clone(),
            incoming_metadata: mutation.metadata.clone(),
            current_value: current.map(|cell| cell.value.clone()),
            current_metadata: current.map(|cell| cell.metadata.clone()),
        })
    }
}
