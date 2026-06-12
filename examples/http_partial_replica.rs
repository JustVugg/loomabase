use std::time::{SystemTime, UNIX_EPOCH};

use loomabase::Result;
use loomabase::auth::encode_token;
use loomabase::client::SqliteClient;
use loomabase::crdt::{CrdtValue, SyncPayload};
use loomabase::replica::{PartialReplicaResponse, ReplicaInterest, ReplicaPredicate};

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_secs()
}

fn transport_error(context: &str, error: impl std::fmt::Display) -> loomabase::SyncError {
    loomabase::SyncError::InvalidPayload(format!("{context}: {error}"))
}

async fn sync_full(
    client: &SqliteClient,
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
) -> Result<()> {
    client
        .sync_with(|payload| async move {
            http.post(endpoint)
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await
                .map_err(|error| transport_error("HTTP transport failed", error))?
                .error_for_status()
                .map_err(|error| transport_error("server rejected full sync", error))?
                .json::<SyncPayload>()
                .await
                .map_err(|error| transport_error("invalid full sync response", error))
        })
        .await?;
    Ok(())
}

async fn sync_partial(
    client: &SqliteClient,
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
    interest: ReplicaInterest,
) -> Result<PartialReplicaResponse> {
    client
        .sync_partial_with("incomplete".into(), interest, |request| async move {
            http.post(endpoint)
                .bearer_auth(token)
                .json(&request)
                .send()
                .await
                .map_err(|error| transport_error("HTTP transport failed", error))?
                .error_for_status()
                .map_err(|error| transport_error("server rejected partial sync", error))?
                .json::<PartialReplicaResponse>()
                .await
                .map_err(|error| transport_error("invalid partial sync response", error))
        })
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    let full_endpoint = std::env::var("LOOMABASE_EXAMPLE_SYNC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080/sync".into());
    let partial_endpoint = std::env::var("LOOMABASE_EXAMPLE_PARTIAL_SYNC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080/sync/partial".into());
    let secret = std::env::var("LOOMABASE_JWT_SECRET").map_err(|_| {
        loomabase::SyncError::InvalidPayload(
            "LOOMABASE_JWT_SECRET must match the running Loomabase server".into(),
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_nanos();
    let row_id = format!("partial-example-{nonce}");
    let writer_path = std::env::temp_dir().join(format!("loomabase-partial-writer-{nonce}.db"));
    let reader_path = std::env::temp_dir().join(format!("loomabase-partial-reader-{nonce}.db"));
    let writer_device = format!("partial-writer-{nonce}");
    let reader_device = format!("partial-reader-{nonce}");
    let tenant = "http-partial-example-tenant";
    let http = reqwest::Client::new();
    let writer_token = encode_token(
        secret.as_bytes(),
        tenant,
        &writer_device,
        now_unix() + 3_600,
    );
    let reader_token = encode_token(
        secret.as_bytes(),
        tenant,
        &reader_device,
        now_unix() + 3_600,
    );
    let writer = SqliteClient::open(&writer_path, &writer_device).await?;
    let reader = SqliteClient::open(&reader_path, &reader_device).await?;
    let interest = ReplicaInterest {
        predicates: vec![ReplicaPredicate::ColumnEquals {
            column: "completed".into(),
            value: CrdtValue::Boolean(false),
        }],
        limit: 10_000,
    };

    writer
        .create_todo(row_id.clone(), "Partial replica membership".into(), false)
        .await?;
    sync_full(&writer, &http, &full_endpoint, &writer_token).await?;
    let joined = sync_partial(
        &reader,
        &http,
        &partial_endpoint,
        &reader_token,
        interest.clone(),
    )
    .await?;
    assert_eq!(joined.member_ids.as_slice(), std::slice::from_ref(&row_id));
    assert!(reader.get_todo(row_id.clone()).await?.is_some());

    writer.update_completed(row_id.clone(), true).await?;
    sync_full(&writer, &http, &full_endpoint, &writer_token).await?;
    let left = sync_partial(&reader, &http, &partial_endpoint, &reader_token, interest).await?;
    assert_eq!(
        left.evicted_row_ids.as_slice(),
        std::slice::from_ref(&row_id)
    );
    assert!(reader.get_todo(row_id).await?.is_none());

    reader
        .remove_partial_replica_scope("incomplete".into())
        .await?;
    let _ = std::fs::remove_file(writer_path);
    let _ = std::fs::remove_file(reader_path);
    println!("Authoritative partial-replica membership and local-only eviction succeeded.");
    Ok(())
}
