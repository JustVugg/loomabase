#![cfg(feature = "server")]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use loomabase::auth::{JwtDeviceAuthenticator, encode_token};
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::http::{
    AuthenticatedDevice, DeviceAuthenticator, HeaderDeviceAuthenticator, ServerConfig, app,
    app_with_config,
};
use loomabase::replica::{
    PartialReplicaRequest, PartialReplicaResponse, ReplicaInterest, ReplicaPredicate,
};
use loomabase::schema::todos_table;
use loomabase::server::initialize_server_schema;
use sqlx_postgres::PgPool;
use tower::ServiceExt;

type BoxError = Box<dyn std::error::Error>;
static HTTP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy)]
struct NotesOnlyAuthenticator;

impl DeviceAuthenticator for NotesOnlyAuthenticator {
    fn authenticate(
        &self,
        _headers: &axum::http::HeaderMap,
    ) -> Result<AuthenticatedDevice, String> {
        Ok(AuthenticatedDevice {
            tenant_id: "authorized-tenant".to_owned(),
            device_id: "authorized-device".to_owned(),
            allowed_tables: Some(["notes".to_owned()].into_iter().collect()),
        })
    }
}

fn title_payload(todo_id: &str, device_id: &str, title: &str) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.to_owned(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: todo_id.to_owned(),
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

fn sync_request(body: Vec<u8>, device_id: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/sync")
        .header("content-type", "application/json");
    if let Some(device_id) = device_id {
        builder = builder
            .header("x-tenant-id", "http-tenant")
            .header("x-device-id", device_id);
    }
    builder.body(Body::from(body)).unwrap()
}

fn partial_sync_request(body: Vec<u8>, device_id: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/sync/partial")
        .header("content-type", "application/json")
        .header("x-tenant-id", "http-tenant")
        .header("x-device-id", device_id)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn sync_endpoint_merges_authenticated_payload() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    sqlx_core::query::query("DELETE FROM todos WHERE id = $1")
        .bind("http-merge-todo")
        .execute(&pool)
        .await?;

    let body = serde_json::to_vec(&title_payload("http-merge-todo", "device-a", "via http"))?;
    let response = app(
        pool.clone(),
        todos_table(),
        Arc::new(HeaderDeviceAuthenticator),
    )
    .oneshot(sync_request(body, Some("device-a")))
    .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let stored: (String,) = sqlx_core::query_as::query_as("SELECT title FROM todos WHERE id = $1")
        .bind("http-merge-todo")
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored.0, "via http");
    Ok(())
}

#[tokio::test]
async fn partial_sync_endpoint_returns_an_authoritative_scope_snapshot() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    let table = todos_table();
    initialize_server_schema(&pool).await?;
    sqlx_core::query::query("DELETE FROM todos WHERE tenant_id = $1 AND id = $2")
        .bind("http-tenant")
        .bind("http-partial-todo")
        .execute(&pool)
        .await?;
    let application = app(pool, table.clone(), Arc::new(HeaderDeviceAuthenticator));

    let seed = serde_json::to_vec(&title_payload("http-partial-todo", "device-a", "partial"))?;
    let response = application
        .clone()
        .oneshot(sync_request(seed, Some("device-a")))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let request = PartialReplicaRequest {
        scope_id: "http-scope".to_owned(),
        scope_version: 1,
        interest: ReplicaInterest {
            predicates: vec![ReplicaPredicate::IdPrefix("http-partial".to_owned())],
            limit: 100,
        },
        known_member_ids: Vec::new(),
        sync: SyncPayload::empty("device-a", 1, &table),
    };
    let response = application
        .oneshot(partial_sync_request(
            serde_json::to_vec(&request)?,
            "device-a",
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let response: PartialReplicaResponse = serde_json::from_slice(&bytes)?;
    assert_eq!(response.member_ids, ["http-partial-todo"]);
    assert_eq!(response.sync.changes.len(), 1);
    Ok(())
}

#[tokio::test]
async fn sync_endpoint_requires_authentication() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;

    let body = serde_json::to_vec(&title_payload("http-auth-todo", "device-a", "anything"))?;
    let response = app(pool, todos_table(), Arc::new(HeaderDeviceAuthenticator))
        .oneshot(sync_request(body, None))
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn sync_endpoint_rejects_schema_mismatch() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;

    let mut payload = title_payload("http-mismatch-todo", "device-a", "wrong schema");
    payload.schema_fingerprint ^= 1; // corrupt the contract fingerprint
    let body = serde_json::to_vec(&payload)?;
    let response = app(pool, todos_table(), Arc::new(HeaderDeviceAuthenticator))
        .oneshot(sync_request(body, Some("device-a")))
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn oversized_request_body_is_rejected() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;

    let config = ServerConfig {
        body_limit_bytes: 32,
        request_timeout: std::time::Duration::from_secs(30),
        ..ServerConfig::default()
    };
    let oversized = vec![b'x'; 1024];
    let response = app_with_config(
        pool,
        todos_table(),
        Arc::new(HeaderDeviceAuthenticator),
        config,
    )
    .oneshot(sync_request(oversized, Some("device-a")))
    .await?;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn bearer_request(body: Vec<u8>, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/sync")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn sync_endpoint_accepts_a_jwt_bearer_token() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    sqlx_core::query::query("DELETE FROM todos WHERE id = $1")
        .bind("http-jwt-todo")
        .execute(&pool)
        .await?;

    let secret = b"server-jwt-secret";
    let token = encode_token(secret, "jwt-tenant", "device-a", unix_now() + 3600);
    let body = serde_json::to_vec(&title_payload("http-jwt-todo", "device-a", "via jwt"))?;
    let response = app(
        pool.clone(),
        todos_table(),
        Arc::new(JwtDeviceAuthenticator::new(secret.to_vec())),
    )
    .oneshot(bearer_request(body, &token))
    .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let stored: (String,) =
        sqlx_core::query_as::query_as("SELECT title FROM todos WHERE tenant_id = $1 AND id = $2")
            .bind("jwt-tenant")
            .bind("http-jwt-todo")
            .fetch_one(&pool)
            .await?;
    assert_eq!(stored.0, "via jwt");
    Ok(())
}

#[tokio::test]
async fn sync_endpoint_rejects_an_expired_jwt() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;

    let secret = b"server-jwt-secret";
    let token = encode_token(secret, "jwt-tenant", "device-a", unix_now() - 10);
    let body = serde_json::to_vec(&title_payload("http-expired-todo", "device-a", "never"))?;
    let response = app(
        pool,
        todos_table(),
        Arc::new(JwtDeviceAuthenticator::new(secret.to_vec())),
    )
    .oneshot(bearer_request(body, &token))
    .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn metrics_endpoint_reports_counters() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    let application = app(pool, todos_table(), Arc::new(HeaderDeviceAuthenticator));

    let body = serde_json::to_vec(&title_payload("http-metrics-todo", "device-a", "metrics"))?;
    let ok = application
        .clone()
        .oneshot(sync_request(body, Some("device-a")))
        .await?;
    assert_eq!(ok.status(), StatusCode::OK);

    let body = serde_json::to_vec(&title_payload("http-metrics-todo", "device-a", "metrics"))?;
    let unauthorized = application
        .clone()
        .oneshot(sync_request(body, None))
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let scrape = application
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(scrape.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(scrape.into_body(), usize::MAX).await?;
    let text = String::from_utf8(bytes.to_vec())?;
    assert!(text.contains("loomabase_sync_requests_total 2"));
    assert!(text.contains("loomabase_sync_ok_total 1"));
    assert!(text.contains("loomabase_auth_failed_total 1"));
    assert!(text.contains("loomabase_sync_in_flight 0"));
    assert!(text.contains("loomabase_sync_duration_seconds_count 2"));
    assert!(text.contains("loomabase_sync_duration_seconds_bucket{le=\"+Inf\"} 2"));
    assert!(text.contains("loomabase_sync_internal_errors_total 0"));
    Ok(())
}

#[tokio::test]
async fn health_endpoint_checks_required_schema_and_permissions() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    let response = app(pool, todos_table(), Arc::new(HeaderDeviceAuthenticator))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn sync_endpoint_enforces_authenticated_table_authorization() -> Result<(), BoxError> {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let Ok(database_url) = std::env::var("LOOMABASE_TEST_DATABASE_URL") else {
        eprintln!("skipping HTTP server test: LOOMABASE_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let pool = PgPool::connect(&database_url).await?;
    initialize_server_schema(&pool).await?;
    let body = serde_json::to_vec(&title_payload("forbidden-todo", "device-a", "never"))?;
    let response = app(pool, todos_table(), Arc::new(NotesOnlyAuthenticator))
        .oneshot(sync_request(body, None))
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    Ok(())
}
