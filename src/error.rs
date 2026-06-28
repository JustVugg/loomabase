use thiserror::Error;

/// Errors exposed by the protocol and persistence adapters.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("PostgreSQL error: {0}")]
    Postgres(#[from] sqlx_core::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("invalid payload: {0}")]
    InvalidPayload(String),

    #[error("schema fingerprint mismatch: payload {actual:#018x}, local contract {expected:#018x}")]
    SchemaMismatch { expected: u64, actual: u64 },

    #[error("Lamport clock overflow")]
    ClockOverflow,

    #[error("SQLite mutex was poisoned by a panic")]
    SqliteLockPoisoned,

    #[error("blocking task failed: {0}")]
    BlockingTask(String),

    #[error("sync page budget exhausted before the client caught up")]
    SyncPageBudgetExhausted,
}

pub type Result<T> = std::result::Result<T, SyncError>;
