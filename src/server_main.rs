//! Minimal Loomabase HTTP server binary (feature `server`).
//!
//! Reads `DATABASE_URL` plus `LOOMABASE_*` authentication, limit, pool, and
//! migration settings documented in the README. Logs are structured via
//! `tracing` (`RUST_LOG` controls verbosity, default `info`) and shutdown is
//! graceful on SIGINT/SIGTERM. Authentication fails closed unless a signed JWT
//! verifier or the explicit development-only header mode is configured.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use loomabase::auth::{
    JwtDeviceAuthenticator, RsaJwtDeviceAuthenticator, SupabaseJwtAuthenticator,
};
use loomabase::http::{
    DeviceAuthenticator, HeaderDeviceAuthenticator, ServerConfig, app_with_config,
};
use loomabase::schema::todos_table;
use loomabase::server::{expire_cursor_leases, initialize_server_schema, rotate_server_epoch};
use sqlx_postgres::PgPool;
use sqlx_postgres::{PgConnectOptions, PgPoolOptions};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,sqlx_postgres::notice=warn")),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL must be set to a PostgreSQL connection string")?;
    let bind = std::env::var("LOOMABASE_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let config = server_config_from_env()?;

    let pool = build_pool(&database_url).await?;
    let migrate_only = env_bool("LOOMABASE_MIGRATE_ONLY")?;
    let skip_schema_init = env_bool("LOOMABASE_SKIP_SCHEMA_INIT")?;
    if migrate_only && skip_schema_init {
        return Err(
            "LOOMABASE_MIGRATE_ONLY and LOOMABASE_SKIP_SCHEMA_INIT are mutually exclusive".into(),
        );
    }
    if migrate_only {
        initialize_server_schema(&pool).await?;
        if env_bool("LOOMABASE_ROTATE_SERVER_EPOCH")? {
            let epoch = rotate_server_epoch(&pool).await?;
            tracing::info!(%epoch, "server epoch rotated");
        }
        if let Some(seconds) = optional_env_parse::<u64>("LOOMABASE_EXPIRE_CURSOR_LEASES_SECS")? {
            let expired = expire_cursor_leases(&pool, Duration::from_secs(seconds)).await?;
            tracing::info!(expired, "inactive cursor leases expired");
        }
        tracing::info!("schema migration completed");
        return Ok(());
    }
    if skip_schema_init {
        tracing::info!("schema initialization skipped; expecting pre-applied migrations");
    } else {
        initialize_server_schema(&pool).await?;
    }
    warn_if_superuser(&pool).await;

    let application = app_with_config(pool, todos_table(), build_authenticator().await?, config);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "loomabase-server listening");
    axum::serve(listener, application)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("loomabase-server stopped");
    Ok(())
}

async fn build_authenticator() -> Result<Arc<dyn DeviceAuthenticator>, Box<dyn std::error::Error>> {
    let audience = nonempty_env("LOOMABASE_JWT_AUDIENCE");
    let issuer = nonempty_env("LOOMABASE_JWT_ISSUER");
    if let Some(supabase_url) =
        nonempty_env("LOOMABASE_SUPABASE_URL").or_else(|| nonempty_env("SUPABASE_URL"))
    {
        let supabase_url = supabase_url.trim_end_matches('/').to_owned();
        let jwks_url = format!("{supabase_url}/auth/v1/.well-known/jwks.json");
        let configured_jwks =
            nonempty_env("LOOMABASE_SUPABASE_JWKS").or_else(|| nonempty_env("SUPABASE_JWKS"));
        let jwks = match configured_jwks {
            Some(jwks) => jwks,
            None => fetch_jwks(&jwks_url).await?,
        };
        let mut authenticator = SupabaseJwtAuthenticator::from_jwks_json(
            &jwks,
            issuer.unwrap_or_else(|| format!("{supabase_url}/auth/v1")),
        )?
        .with_audience(Some(audience.unwrap_or_else(|| "authenticated".to_owned())));
        if let Some(path) = nonempty_env("LOOMABASE_SUPABASE_TENANT_CLAIM") {
            authenticator = authenticator.with_tenant_claim(path);
        }
        if let Some(path) = nonempty_env("LOOMABASE_SUPABASE_TABLES_CLAIM") {
            authenticator = authenticator.with_tables_claim(path);
        }
        if nonempty_env("LOOMABASE_SUPABASE_JWKS").is_none()
            && nonempty_env("SUPABASE_JWKS").is_none()
        {
            let refresh_seconds = env_parse("LOOMABASE_JWKS_REFRESH_SECS", 600_u64)?;
            if refresh_seconds == 0 {
                return Err("LOOMABASE_JWKS_REFRESH_SECS must be greater than zero".into());
            }
            spawn_jwks_refresh(authenticator.clone(), jwks_url, refresh_seconds);
        }
        tracing::info!("device authentication: Supabase asymmetric JWKS");
        return Ok(Arc::new(authenticator));
    }
    if let Ok(pem) = std::env::var("LOOMABASE_JWT_PUBLIC_KEY")
        && !pem.is_empty()
    {
        tracing::info!("device authentication: RS256 JWT (RSA public key)");
        let mut authenticator = RsaJwtDeviceAuthenticator::from_public_key_pem(&pem)?;
        if let Some(audience) = audience.as_deref() {
            authenticator = authenticator.with_audience(audience);
        }
        if let Some(issuer) = issuer.as_deref() {
            authenticator = authenticator.with_issuer(issuer);
        }
        return Ok(Arc::new(authenticator));
    }
    if let Ok(secret) = std::env::var("LOOMABASE_JWT_SECRET")
        && !secret.is_empty()
    {
        if secret.len() < 32 {
            return Err("LOOMABASE_JWT_SECRET must contain at least 32 bytes".into());
        }
        tracing::info!("device authentication: HS256 JWT");
        let mut authenticator = JwtDeviceAuthenticator::new(secret.into_bytes());
        if let Some(audience) = audience.as_deref() {
            authenticator = authenticator.with_audience(audience);
        }
        if let Some(issuer) = issuer.as_deref() {
            authenticator = authenticator.with_issuer(issuer);
        }
        return Ok(Arc::new(authenticator));
    }
    if env_bool("LOOMABASE_ALLOW_INSECURE_HEADERS")? {
        tracing::warn!("device authentication: insecure development headers explicitly enabled");
        Ok(Arc::new(HeaderDeviceAuthenticator))
    } else {
        Err(
            "configure LOOMABASE_SUPABASE_URL, LOOMABASE_JWT_PUBLIC_KEY, or \
             LOOMABASE_JWT_SECRET; insecure headers require LOOMABASE_ALLOW_INSECURE_HEADERS=true"
                .into(),
        )
    }
}

fn server_config_from_env() -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let mut config = ServerConfig::default();
    if let Ok(value) = std::env::var("LOOMABASE_BODY_LIMIT_BYTES") {
        config.body_limit_bytes = value
            .parse()
            .map_err(|_| "LOOMABASE_BODY_LIMIT_BYTES must be a positive integer")?;
    }
    if let Ok(value) = std::env::var("LOOMABASE_REQUEST_TIMEOUT_SECS") {
        let secs: u64 = value
            .parse()
            .map_err(|_| "LOOMABASE_REQUEST_TIMEOUT_SECS must be a non-negative integer")?;
        config.request_timeout = Duration::from_secs(secs);
    }
    if let Ok(value) = std::env::var("LOOMABASE_MAX_CONCURRENT_REQUESTS") {
        config.max_concurrent_requests = value
            .parse()
            .map_err(|_| "LOOMABASE_MAX_CONCURRENT_REQUESTS must be a positive integer")?;
    }
    if let Ok(value) = std::env::var("LOOMABASE_STATEMENT_TIMEOUT_SECS") {
        config.statement_timeout = Duration::from_secs(
            value
                .parse()
                .map_err(|_| "LOOMABASE_STATEMENT_TIMEOUT_SECS must be a positive integer")?,
        );
    }
    if let Ok(value) = std::env::var("LOOMABASE_LOCK_TIMEOUT_SECS") {
        config.lock_timeout = Duration::from_secs(
            value
                .parse()
                .map_err(|_| "LOOMABASE_LOCK_TIMEOUT_SECS must be a positive integer")?,
        );
    }
    if config.body_limit_bytes == 0
        || config.max_concurrent_requests == 0
        || config.request_timeout.is_zero()
        || config.statement_timeout.is_zero()
        || config.lock_timeout.is_zero()
    {
        return Err("server limits and timeouts must be greater than zero".into());
    }
    Ok(config)
}

async fn build_pool(database_url: &str) -> Result<PgPool, Box<dyn std::error::Error>> {
    let max_connections = env_parse("LOOMABASE_DB_MAX_CONNECTIONS", 20_u32)?;
    let min_connections = env_parse("LOOMABASE_DB_MIN_CONNECTIONS", 1_u32)?;
    let acquire_timeout_secs = env_parse("LOOMABASE_DB_ACQUIRE_TIMEOUT_SECS", 10_u64)?;
    if max_connections == 0 || min_connections > max_connections || acquire_timeout_secs == 0 {
        return Err("invalid PostgreSQL pool limits".into());
    }
    let mut connect_options = PgConnectOptions::from_str(database_url)?;
    if env_bool("LOOMABASE_DB_TRANSACTION_POOLER")? || database_url.contains(":6543/") {
        // Supabase Supavisor transaction mode does not support prepared
        // statements. SQLx's statement cache must therefore be disabled.
        connect_options = connect_options.statement_cache_capacity(0);
        tracing::info!("PostgreSQL prepared statements disabled for transaction pooler");
    }
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(min_connections)
        .acquire_timeout(Duration::from_secs(acquire_timeout_secs))
        .connect_with(connect_options)
        .await?)
}

async fn fetch_jwks(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

fn spawn_jwks_refresh(authenticator: SupabaseJwtAuthenticator, url: String, seconds: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(seconds));
        interval.tick().await;
        loop {
            interval.tick().await;
            match fetch_jwks(&url).await {
                Ok(jwks) => match authenticator.replace_jwks_json(&jwks) {
                    Ok(()) => tracing::info!("Supabase JWKS refreshed"),
                    Err(error) => tracing::warn!(%error, "Supabase JWKS refresh rejected"),
                },
                Err(error) => tracing::warn!(%error, "Supabase JWKS refresh failed"),
            }
        }
    });
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn env_parse<T>(name: &str, default: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| format!("{name} has an invalid value").into()),
        Err(_) => Ok(default),
    }
}

fn optional_env_parse<T>(name: &str) -> Result<Option<T>, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map(Some)
            .map_err(|_| format!("{name} has an invalid value").into()),
        Err(_) => Ok(None),
    }
}

fn env_bool(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    match std::env::var(name).as_deref() {
        Ok("true" | "1") => Ok(true),
        Ok("false" | "0") | Err(_) => Ok(false),
        Ok(_) => Err(format!("{name} must be true/false or 1/0").into()),
    }
}

async fn warn_if_superuser(pool: &PgPool) {
    match sqlx_core::query_scalar::query_scalar::<_, bool>(
        "SELECT current_setting('is_superuser')::boolean",
    )
    .fetch_one(pool)
    .await
    {
        Ok(true) => tracing::warn!(
            "connected to PostgreSQL as a superuser: Row-Level Security is bypassed; \
             use a dedicated non-superuser role in production"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "could not determine superuser status"),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
