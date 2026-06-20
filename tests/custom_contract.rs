use std::collections::BTreeMap;
use std::sync::Arc;

use loomabase::Result;
use loomabase::client::SqliteClient;
use loomabase::crdt::{CrdtState, CrdtValue};
use loomabase::schema::{ColumnDef, ColumnType, TableDef};
use tokio::sync::Mutex;

fn notes_table() -> TableDef {
    TableDef::new(
        "notes",
        vec![
            ColumnDef::new("body", ColumnType::Text),
            ColumnDef::new("priority", ColumnType::Integer),
            ColumnDef::new("pinned", ColumnType::Boolean),
        ],
    )
    .unwrap()
}

async fn sync(client: &SqliteClient, server: &Arc<Mutex<CrdtState>>) -> Result<()> {
    let server = Arc::clone(server);
    client
        .sync_with(move |payload| async move {
            let device_id = payload.source_device_id.clone();
            server.lock().await.merge(payload, &device_id)
        })
        .await?;
    Ok(())
}

async fn converge(
    a: &SqliteClient,
    b: &SqliteClient,
    server: &Arc<Mutex<CrdtState>>,
) -> Result<()> {
    sync(a, server).await?;
    sync(b, server).await?;
    sync(a, server).await?;
    sync(b, server).await?;
    Ok(())
}

#[tokio::test]
async fn custom_contract_syncs_scalar_columns_end_to_end() -> Result<()> {
    let table = notes_table();
    let a = SqliteClient::open_with(":memory:", "device-a", table.clone()).await?;
    let b = SqliteClient::open_with(":memory:", "device-b", table.clone()).await?;
    let server = Arc::new(Mutex::new(CrdtState::new(table)));

    a.insert(
        "note-1".into(),
        BTreeMap::from([
            ("body".into(), CrdtValue::Text("first note".into())),
            ("priority".into(), CrdtValue::Integer(5)),
            ("pinned".into(), CrdtValue::Boolean(true)),
        ]),
    )
    .await?;
    converge(&a, &b, &server).await?;

    // The Integer, Text and Boolean columns all round-trip to the other device.
    assert_eq!(
        b.get_cell("note-1".into(), "priority".into()).await?,
        Some(CrdtValue::Integer(5))
    );
    assert_eq!(
        b.get_cell("note-1".into(), "body".into()).await?,
        Some(CrdtValue::Text("first note".into()))
    );
    assert_eq!(
        b.get_cell("note-1".into(), "pinned".into()).await?,
        Some(CrdtValue::Boolean(true))
    );

    // A column-level edit on B converges back to A.
    b.set("note-1".into(), "priority".into(), CrdtValue::Integer(9))
        .await?;
    converge(&a, &b, &server).await?;
    assert_eq!(
        a.get_cell("note-1".into(), "priority".into()).await?,
        Some(CrdtValue::Integer(9))
    );

    // Deletion converges through the generic liveness register.
    a.delete("note-1".into()).await?;
    converge(&a, &b, &server).await?;
    assert_eq!(b.get_cell("note-1".into(), "priority".into()).await?, None);
    Ok(())
}

#[tokio::test]
async fn custom_contract_rejects_wrong_types_and_unknown_columns() -> Result<()> {
    let table = notes_table();
    let client = SqliteClient::open_with(":memory:", "device-a", table).await?;
    client
        .insert(
            "note-1".into(),
            BTreeMap::from([("priority".into(), CrdtValue::Integer(1))]),
        )
        .await?;

    // Wrong value type for the column's declared domain.
    assert!(
        client
            .set(
                "note-1".into(),
                "priority".into(),
                CrdtValue::Text("nope".into())
            )
            .await
            .is_err()
    );
    // Column absent from the contract.
    assert!(
        client
            .set("note-1".into(), "secret".into(), CrdtValue::Integer(1))
            .await
            .is_err()
    );
    // Reserved liveness register is not writable through the generic API.
    assert!(
        client
            .set("note-1".into(), "deleted".into(), CrdtValue::Boolean(true))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn unprovided_columns_take_their_schema_default() -> Result<()> {
    let table = notes_table();
    let client = SqliteClient::open_with(":memory:", "device-a", table).await?;
    client
        .insert(
            "note-1".into(),
            BTreeMap::from([("body".into(), CrdtValue::Text("only body".into()))]),
        )
        .await?;

    assert_eq!(
        client.get_cell("note-1".into(), "priority".into()).await?,
        Some(CrdtValue::Integer(0))
    );
    assert_eq!(
        client.get_cell("note-1".into(), "pinned".into()).await?,
        Some(CrdtValue::Boolean(false))
    );
    Ok(())
}

#[tokio::test]
async fn schema_fingerprint_mismatch_is_rejected_before_mutation() -> Result<()> {
    // A client on the canonical todos contract produces a real delta.
    let client = SqliteClient::open(":memory:", "device-a").await?;
    client
        .create_todo("todo-1".into(), "task".into(), false)
        .await?;
    let outbound = client.local_delta().await?;

    // A reference server bound to a different contract must reject it untouched.
    let mut server = CrdtState::new(notes_table());
    let before = server.clone();
    assert!(server.merge(outbound, "device-a").is_err());
    assert_eq!(server, before);
    Ok(())
}
