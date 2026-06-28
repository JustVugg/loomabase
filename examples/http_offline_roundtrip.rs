use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use loomabase::Result;
use loomabase::auth::encode_token;
use loomabase::client::{SqliteClient, Todo};
use loomabase::crdt::SyncPayload;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_secs()
}

async fn sync_http(
    client: &SqliteClient,
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
) -> Result<()> {
    client
        .sync_with(|payload| async move {
            let response = http
                .post(endpoint)
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await
                .map_err(|error| {
                    loomabase::SyncError::InvalidPayload(format!("HTTP transport failed: {error}"))
                })?;
            let response = response.error_for_status().map_err(|error| {
                loomabase::SyncError::InvalidPayload(format!(
                    "Loomabase server rejected synchronization: {error}"
                ))
            })?;
            response.json::<SyncPayload>().await.map_err(|error| {
                loomabase::SyncError::InvalidPayload(format!(
                    "invalid Loomabase server response: {error}"
                ))
            })
        })
        .await?;
    Ok(())
}

fn database_path(device: &str) -> PathBuf {
    std::env::temp_dir().join(format!("loomabase-http-example-{device}.db"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let endpoint = std::env::var("LOOMABASE_EXAMPLE_SYNC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080/sync".into());
    let secret = std::env::var("LOOMABASE_JWT_SECRET").map_err(|_| {
        loomabase::SyncError::InvalidPayload(
            "LOOMABASE_JWT_SECRET must match the running Loomabase server".into(),
        )
    })?;
    let tenant = "http-example-tenant";
    let path_a = database_path("a");
    let path_b = database_path("b");
    for path in [&path_a, &path_b] {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
    }

    let http = reqwest::Client::new();
    let token_a = encode_token(secret.as_bytes(), tenant, "device-a", now_unix() + 3_600);
    let token_b = encode_token(secret.as_bytes(), tenant, "device-b", now_unix() + 3_600);

    let device_a = SqliteClient::open(&path_a, "device-a").await?;
    let device_b = SqliteClient::open(&path_b, "device-b").await?;
    device_a
        .create_todo("todo-http-1".into(), "Initial title".into(), false)
        .await?;
    sync_http(&device_a, &http, &endpoint, &token_a).await?;
    sync_http(&device_b, &http, &endpoint, &token_b).await?;

    device_a
        .update_title("todo-http-1".into(), "Edited offline on A".into())
        .await?;
    device_b
        .update_completed("todo-http-1".into(), true)
        .await?;
    sync_http(&device_b, &http, &endpoint, &token_b).await?;
    sync_http(&device_a, &http, &endpoint, &token_a).await?;

    // Simulate a process restart and prove that the SQLite replica resumes safely.
    drop(device_a);
    let device_a = SqliteClient::open(&path_a, "device-a").await?;
    sync_http(&device_a, &http, &endpoint, &token_a).await?;
    sync_http(&device_b, &http, &endpoint, &token_b).await?;

    let expected = Todo {
        id: "todo-http-1".into(),
        title: "Edited offline on A".into(),
        completed: true,
    };
    assert_eq!(
        device_a.get_todo("todo-http-1".into()).await?,
        Some(expected.clone())
    );
    assert_eq!(
        device_b.get_todo("todo-http-1".into()).await?,
        Some(expected)
    );

    println!("Persistent SQLite replicas converged through the authenticated HTTP server.");
    Ok(())
}
