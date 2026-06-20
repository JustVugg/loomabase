use loomabase::Result;
use loomabase::client::{
    acknowledge_local_delta, apply_remote_payload, get_local_delta, initialize_client, read_todo,
};
use loomabase::crdt::{CrdtState, SyncPayload};
use loomabase::schema::todos_table;
use rusqlite::{Connection, TransactionBehavior, params};

fn local_delta(connection: &mut Connection) -> Result<SyncPayload> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let payload = get_local_delta(&tx, &todos_table())?;
    tx.commit()?;
    Ok(payload)
}

fn acknowledge(connection: &mut Connection, payload: &SyncPayload) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    acknowledge_local_delta(&tx, payload, &todos_table())?;
    tx.commit()?;
    Ok(())
}

fn apply_remote(connection: &mut Connection, payload: &SyncPayload) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    apply_remote_payload(&tx, payload, &todos_table())?;
    tx.commit()?;
    Ok(())
}

#[test]
fn offline_devices_converge_and_merge_is_idempotent() -> Result<()> {
    let mut device_a = Connection::open_in_memory()?;
    let mut device_b = Connection::open_in_memory()?;
    initialize_client(&mut device_a, "device-a")?;
    initialize_client(&mut device_b, "device-b")?;
    let mut server = CrdtState::default();

    // A creates the baseline; B receives it before both devices go offline.
    device_a.execute(
        "INSERT INTO todos(id, title, completed) VALUES (?1, ?2, ?3)",
        params!["todo-1", "Initial title", false],
    )?;
    let baseline = local_delta(&mut device_a)?;
    let response_for_a = server.merge(baseline.clone(), "device-a")?;
    acknowledge(&mut device_a, &baseline)?;
    apply_remote(&mut device_a, &response_for_a)?;

    let empty_b = local_delta(&mut device_b)?;
    let baseline_for_b = server.merge(empty_b, "device-b")?;
    apply_remote(&mut device_b, &baseline_for_b)?;

    // The devices modify different columns, so neither update may be lost.
    device_a.execute(
        "UPDATE todos SET title = ?2 WHERE id = ?1",
        params!["todo-1", "Title from A"],
    )?;
    device_b.execute(
        "UPDATE todos SET completed = ?2 WHERE id = ?1",
        params!["todo-1", true],
    )?;
    let delta_a = local_delta(&mut device_a)?;
    let delta_b = local_delta(&mut device_b)?;

    let response_a = server.merge(delta_a.clone(), "device-a")?;
    let response_b = server.merge(delta_b.clone(), "device-b")?;
    acknowledge(&mut device_a, &delta_a)?;
    acknowledge(&mut device_b, &delta_b)?;
    apply_remote(&mut device_a, &response_a)?;
    apply_remote(&mut device_b, &response_b)?;

    // A subsequent pull delivers B's update to A.
    let pull_a = local_delta(&mut device_a)?;
    let response_pull_a = server.merge(pull_a, "device-a")?;
    apply_remote(&mut device_a, &response_pull_a)?;

    assert_eq!(
        read_todo(&device_a, "todo-1")?,
        Some(("Title from A".to_owned(), true))
    );
    assert_eq!(
        read_todo(&device_b, "todo-1")?,
        Some(("Title from A".to_owned(), true))
    );

    let state_after_first_merge = server.clone();
    let repeated_response = server.merge(delta_b, "device-b")?;
    assert_eq!(server.cells, state_after_first_merge.cells);
    assert!(repeated_response.changes.iter().all(|row| {
        row.columns
            .values()
            .all(|column| server.cells.values().any(|current| current == column))
    }));
    Ok(())
}

#[test]
fn lww_tie_break_uses_device_id_and_rejects_spoofing() -> Result<()> {
    use std::collections::BTreeMap;

    use loomabase::crdt::{ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange};

    let mut server = CrdtState::default();
    let payload_a = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "device-a".to_owned(),
        source_lamport: 7,
        changes: vec![RowChange {
            todo_id: "todo-1".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text("A".to_owned()),
                    metadata: ColumnMetadata {
                        lamport_clock: 7,
                        device_id: "device-a".to_owned(),
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
    };
    let mut payload_b = payload_a.clone();
    payload_b.source_device_id = "device-b".to_owned();
    payload_b.changes[0].columns.get_mut("title").unwrap().value = CrdtValue::Text("B".to_owned());
    payload_b.changes[0]
        .columns
        .get_mut("title")
        .unwrap()
        .metadata
        .device_id = "device-b".to_owned();

    server.merge(payload_a.clone(), "device-a")?;
    server.merge(payload_b, "device-b")?;
    assert_eq!(
        server.cells[&("todo-1".to_owned(), "title".to_owned())].value,
        CrdtValue::Text("B".to_owned())
    );
    assert!(server.merge(payload_a, "device-b").is_err());
    Ok(())
}
