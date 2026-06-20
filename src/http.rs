//! Optional HTTP server surface (enabled by the `server` feature).
//!
//! This is a thin, transport-only layer over [`crate::server::merge_crdt_states`].
//! Device authentication is pluggable. The reference server binary selects a
//! signed JWT verifier and fails closed unless the bundled development-only
//! [`HeaderDeviceAuthenticator`] is explicitly enabled.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use sqlx_postgres::PgPool;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::crdt::SyncPayload;
use crate::error::SyncError;
use crate::policy::SyncSecurity;
use crate::replica::{PartialReplicaRequest, PartialReplicaResponse};
use crate::schema::TableDef;
use crate::server::{merge_crdt_states_with_security, merge_partial_replica_with_security};

const DURATION_BUCKETS: [(u64, &str); 9] = [
    (50_000, "0.05"),
    (100_000, "0.1"),
    (250_000, "0.25"),
    (500_000, "0.5"),
    (1_000_000, "1"),
    (2_500_000, "2.5"),
    (5_000_000, "5"),
    (10_000_000, "10"),
    (30_000_000, "30"),
];

/// Tunable limits for the HTTP surface. The protocol's own row/version caps
/// bound the payload further; these reject abuse before any work is done.
#[derive(Clone, Copy, Debug)]
pub struct ServerConfig {
    /// Maximum accepted request body in bytes (oversized bodies get a `413`).
    pub body_limit_bytes: usize,
    /// Maximum time a single request may run before a `408`.
    pub request_timeout: Duration,
    /// Maximum number of requests executing concurrently in one process.
    pub max_concurrent_requests: usize,
    /// `PostgreSQL` statement timeout applied inside each sync transaction.
    pub statement_timeout: Duration,
    /// `PostgreSQL` lock timeout applied inside each sync transaction.
    pub lock_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // Large enough for one maximum-sized text value plus its JSON
            // envelope, while still rejecting unbounded bodies before parsing.
            body_limit_bytes: 5 << 20,
            request_timeout: Duration::from_secs(30),
            max_concurrent_requests: 128,
            statement_timeout: Duration::from_secs(25),
            lock_timeout: Duration::from_secs(5),
        }
    }
}

/// The authenticated identity behind a sync request. The `tenant_id` is the
/// isolation boundary; the `device_id` is the CRDT write attribution.
#[derive(Clone, Debug)]
pub struct AuthenticatedDevice {
    pub tenant_id: String,
    pub device_id: String,
    /// Optional allow-list carried by trusted authentication claims.
    pub allowed_tables: Option<BTreeSet<String>>,
}

impl AuthenticatedDevice {
    #[must_use]
    pub fn can_sync_table(&self, table: &str) -> bool {
        self.allowed_tables
            .as_ref()
            .is_none_or(|allowed| allowed.contains(table))
    }
}

/// Authenticates a request and returns its tenant and device identity.
/// Implementations must reject unauthenticated requests with an error message.
pub trait DeviceAuthenticator: Send + Sync + 'static {
    /// Returns the authenticated identity, or an error message for a `401`.
    ///
    /// # Errors
    /// Returns an error message when the request is not authenticated.
    fn authenticate(&self, headers: &HeaderMap) -> Result<AuthenticatedDevice, String>;
}

/// Development stub: trusts the `x-tenant-id` and `x-device-id` headers.
/// **Not for production** — a real authenticator verifies a signed token.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeaderDeviceAuthenticator;

impl DeviceAuthenticator for HeaderDeviceAuthenticator {
    fn authenticate(&self, headers: &HeaderMap) -> Result<AuthenticatedDevice, String> {
        Ok(AuthenticatedDevice {
            tenant_id: required_header(headers, "x-tenant-id")?,
            device_id: required_header(headers, "x-device-id")?,
            allowed_tables: None,
        })
    }
}

pub(crate) fn required_header(headers: &HeaderMap, name: &str) -> Result<String, String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing or invalid {name} header"))
}

/// Process-lifetime request counters exposed at `GET /metrics`.
#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    sync_ok: AtomicU64,
    sync_rejected: AtomicU64,
    auth_failed: AtomicU64,
    internal_errors: AtomicU64,
    in_flight: AtomicU64,
    duration_micros: AtomicU64,
    duration_count: AtomicU64,
    duration_buckets: [AtomicU64; DURATION_BUCKETS.len()],
}

impl Metrics {
    fn render(&self) -> String {
        let requests = self.requests.load(Ordering::Relaxed);
        let ok = self.sync_ok.load(Ordering::Relaxed);
        let rejected = self.sync_rejected.load(Ordering::Relaxed);
        let auth_failed = self.auth_failed.load(Ordering::Relaxed);
        let internal_errors = self.internal_errors.load(Ordering::Relaxed);
        let in_flight = self.in_flight.load(Ordering::Relaxed);
        let duration_micros = self.duration_micros.load(Ordering::Relaxed);
        let duration_count = self.duration_count.load(Ordering::Relaxed);
        let duration_seconds = format!(
            "{}.{:06}",
            duration_micros / 1_000_000,
            duration_micros % 1_000_000
        );
        let mut duration_buckets = String::new();
        for ((_, label), count) in DURATION_BUCKETS.iter().zip(&self.duration_buckets) {
            let _ = writeln!(
                duration_buckets,
                "loomabase_sync_duration_seconds_bucket{{le=\"{label}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
        format!(
            "# HELP loomabase_sync_requests_total Sync requests received.\n\
             # TYPE loomabase_sync_requests_total counter\n\
             loomabase_sync_requests_total {requests}\n\
             # HELP loomabase_sync_ok_total Sync requests that merged successfully.\n\
             # TYPE loomabase_sync_ok_total counter\n\
             loomabase_sync_ok_total {ok}\n\
             # HELP loomabase_sync_rejected_total Authenticated sync requests rejected by the merge.\n\
             # TYPE loomabase_sync_rejected_total counter\n\
             loomabase_sync_rejected_total {rejected}\n\
             # HELP loomabase_auth_failed_total Requests rejected by authentication.\n\
             # TYPE loomabase_auth_failed_total counter\n\
             loomabase_auth_failed_total {auth_failed}\n\
             # HELP loomabase_sync_internal_errors_total Sync requests that failed internally.\n\
             # TYPE loomabase_sync_internal_errors_total counter\n\
             loomabase_sync_internal_errors_total {internal_errors}\n\
             # HELP loomabase_sync_in_flight Sync requests currently executing.\n\
             # TYPE loomabase_sync_in_flight gauge\n\
             loomabase_sync_in_flight {in_flight}\n\
             # HELP loomabase_sync_duration_seconds Sync request execution time.\n\
             # TYPE loomabase_sync_duration_seconds histogram\n\
             {duration_buckets}\
             loomabase_sync_duration_seconds_bucket{{le=\"+Inf\"}} {duration_count}\n\
             loomabase_sync_duration_seconds_sum {duration_seconds}\n\
             loomabase_sync_duration_seconds_count {duration_count}\n"
        )
    }
}

struct SyncObservation {
    metrics: Arc<Metrics>,
    started_at: Instant,
}

impl SyncObservation {
    fn start(metrics: Arc<Metrics>) -> Self {
        metrics.in_flight.fetch_add(1, Ordering::Relaxed);
        Self {
            metrics,
            started_at: Instant::now(),
        }
    }
}

impl Drop for SyncObservation {
    fn drop(&mut self) {
        let micros = u64::try_from(self.started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.metrics
            .duration_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.metrics.duration_count.fetch_add(1, Ordering::Relaxed);
        for ((threshold, _), bucket) in DURATION_BUCKETS.iter().zip(&self.metrics.duration_buckets)
        {
            if micros <= *threshold {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    table: Arc<TableDef>,
    authenticator: Arc<dyn DeviceAuthenticator>,
    security: Arc<SyncSecurity>,
    metrics: Arc<Metrics>,
    config: ServerConfig,
}

/// Builds the Loomabase HTTP application with default limits.
pub fn app(pool: PgPool, table: TableDef, authenticator: Arc<dyn DeviceAuthenticator>) -> Router {
    app_with_config(pool, table, authenticator, ServerConfig::default())
}

/// Builds the application with explicit limits and HTTP request tracing:
/// `POST /sync` and `GET /health`.
pub fn app_with_config(
    pool: PgPool,
    table: TableDef,
    authenticator: Arc<dyn DeviceAuthenticator>,
    config: ServerConfig,
) -> Router {
    app_with_config_and_security(
        pool,
        table,
        authenticator,
        config,
        Arc::new(SyncSecurity::default()),
    )
}

/// Builds the application with explicit security hooks. This is the recommended
/// production integration point for field authorization and business validation.
pub fn app_with_config_and_security(
    pool: PgPool,
    table: TableDef,
    authenticator: Arc<dyn DeviceAuthenticator>,
    config: ServerConfig,
    security: Arc<SyncSecurity>,
) -> Router {
    let state = AppState {
        pool,
        table: Arc::new(table),
        authenticator,
        security,
        metrics: Arc::new(Metrics::default()),
        config,
    };
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/sync", post(sync))
        .route("/sync/partial", post(sync_partial))
        .layer(DefaultBodyLimit::max(config.body_limit_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            config.request_timeout,
        ))
        .layer(ConcurrencyLimitLayer::new(config.max_concurrent_requests))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Response {
    let app_table = state.table.name();
    let crdt_table = state.table.crdt_table();
    // Catalog lookups avoid taking relation locks that can deadlock with a
    // concurrent migration while still proving every required relation exists.
    let tables_ready = sqlx_core::query_scalar::query_scalar::<_, bool>(
        "SELECT to_regclass($1) IS NOT NULL
            AND to_regclass($2) IS NOT NULL
            AND to_regclass($3) IS NOT NULL
            AND to_regclass($4) IS NOT NULL
            AND to_regclass($5) IS NOT NULL
            AND to_regclass($6) IS NOT NULL",
    )
    .bind("loomabase_state")
    .bind("loomabase_cursor_lease")
    .bind("loomabase_server_state")
    .bind("loomabase_audit_log")
    .bind(app_table)
    .bind(&crdt_table)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false);
    let sequence_ready = sqlx_core::query_scalar::query_scalar::<_, bool>(
        "SELECT has_sequence_privilege(current_user, 'loomabase_seq', 'USAGE')",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false);
    if tables_ready && sequence_ready {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
    }
}

async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
        .into_response()
}

async fn sync(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncPayload>,
) -> Response {
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let _observation = SyncObservation::start(Arc::clone(&state.metrics));
    let identity = match state.authenticator.authenticate(&headers) {
        Ok(identity) => identity,
        Err(message) => {
            state.metrics.auth_failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(reason = %message, "sync authentication rejected");
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    if !identity.can_sync_table(state.table.name()) {
        state.metrics.sync_rejected.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            tenant = %identity.tenant_id,
            device = %identity.device_id,
            table = state.table.name(),
            "sync authorization rejected"
        );
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let changes_in = payload.changes.len();
    match run_merge(&state, payload, &identity).await {
        Ok(response) => {
            state.metrics.sync_ok.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                tenant = %identity.tenant_id,
                device = %identity.device_id,
                changes_in,
                changes_out = response.changes.len(),
                "sync merged"
            );
            Json(response).into_response()
        }
        Err(error) => {
            let (status, message) = error_response(&error);
            if status.is_server_error() {
                state
                    .metrics
                    .internal_errors
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                state.metrics.sync_rejected.fetch_add(1, Ordering::Relaxed);
            }
            tracing::warn!(
                tenant = %identity.tenant_id,
                device = %identity.device_id,
                %status,
                error = %error,
                "sync rejected"
            );
            (status, message).into_response()
        }
    }
}

async fn run_merge(
    state: &AppState,
    payload: SyncPayload,
    identity: &AuthenticatedDevice,
) -> Result<SyncPayload, SyncError> {
    let mut tx = state.pool.begin().await?;
    set_transaction_timeout(&mut tx, "statement_timeout", state.config.statement_timeout).await?;
    set_transaction_timeout(&mut tx, "lock_timeout", state.config.lock_timeout).await?;
    let response = merge_crdt_states_with_security(
        &mut tx,
        payload,
        &identity.device_id,
        &identity.tenant_id,
        state.table.as_ref(),
        state.security.as_ref(),
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}

async fn sync_partial(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PartialReplicaRequest>,
) -> Response {
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let _observation = SyncObservation::start(Arc::clone(&state.metrics));
    let identity = match state.authenticator.authenticate(&headers) {
        Ok(identity) => identity,
        Err(message) => {
            state.metrics.auth_failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(reason = %message, "partial sync authentication rejected");
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    if !identity.can_sync_table(state.table.name()) {
        state.metrics.sync_rejected.fetch_add(1, Ordering::Relaxed);
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let changes_in = request.sync.changes.len();
    match run_partial_merge(&state, request, &identity).await {
        Ok(response) => {
            state.metrics.sync_ok.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                tenant = %identity.tenant_id,
                device = %identity.device_id,
                changes_in,
                members = response.member_ids.len(),
                evictions = response.evicted_row_ids.len(),
                "partial sync merged"
            );
            Json(response).into_response()
        }
        Err(error) => {
            let (status, message) = error_response(&error);
            if status.is_server_error() {
                state
                    .metrics
                    .internal_errors
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                state.metrics.sync_rejected.fetch_add(1, Ordering::Relaxed);
            }
            tracing::warn!(
                tenant = %identity.tenant_id,
                device = %identity.device_id,
                %status,
                error = %error,
                "partial sync rejected"
            );
            (status, message).into_response()
        }
    }
}

async fn run_partial_merge(
    state: &AppState,
    request: PartialReplicaRequest,
    identity: &AuthenticatedDevice,
) -> Result<PartialReplicaResponse, SyncError> {
    let mut tx = state.pool.begin().await?;
    set_transaction_timeout(&mut tx, "statement_timeout", state.config.statement_timeout).await?;
    set_transaction_timeout(&mut tx, "lock_timeout", state.config.lock_timeout).await?;
    let response = merge_partial_replica_with_security(
        &mut tx,
        request,
        &identity.device_id,
        &identity.tenant_id,
        state.table.as_ref(),
        state.security.as_ref(),
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}

async fn set_transaction_timeout(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    setting: &str,
    timeout: Duration,
) -> Result<(), SyncError> {
    let millis = timeout.as_millis().to_string();
    sqlx_core::query::query("SELECT set_config($1, $2, true)")
        .bind(setting)
        .bind(millis)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Maps a [`SyncError`] to an HTTP status. Untrusted-input failures are client
/// errors; everything else is a server error.
fn error_response(error: &SyncError) -> (StatusCode, String) {
    match error {
        SyncError::InvalidPayload(_) | SyncError::SchemaMismatch { .. } => {
            (StatusCode::BAD_REQUEST, error.to_string())
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error".to_owned(),
        ),
    }
}
