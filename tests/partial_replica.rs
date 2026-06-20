use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::client::{
    apply_partial_replica_response, get_partial_replica_request, initialize_client, read_todo,
    remove_partial_replica_scope,
};
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtState, CrdtValue, DELETED_COLUMN, PROTOCOL_VERSION, RowChange,
    SyncPayload,
};
use loomabase::replica::{PartialReplicaRequest, ReplicaInterest, ReplicaPredicate};
use loomabase::schema::todos_table;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

fn server_write(
    server: &mut CrdtState,
    device: &str,
    clock: u64,
    row_id: &str,
    columns: BTreeMap<String, CrdtValue>,
) -> Result<()> {
    let payload = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device.to_owned(),
        source_lamport: clock,
        changes: vec![RowChange {
            todo_id: row_id.to_owned(),
            columns: columns
                .into_iter()
                .map(|(name, value)| {
                    (
                        name,
                        CrdtColumn {
                            value,
                            metadata: ColumnMetadata {
                                lamport_clock: clock,
                                device_id: device.to_owned(),
                            },
                        },
                    )
                })
                .collect(),
        }],
        cursor: 0,
        has_more: false,
        cursor_reset: false,
        cursor_token: None,
        server_epoch: None,
        rejections: Vec::new(),
    };
    server.merge(payload, device)?;
    Ok(())
}

fn request(
    connection: &mut Connection,
    scope: &str,
    interest: ReplicaInterest,
) -> Result<PartialReplicaRequest> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let request = get_partial_replica_request(&tx, &todos_table(), scope, interest)?;
    tx.commit()?;
    Ok(request)
}

fn apply(
    connection: &mut Connection,
    sent: &PartialReplicaRequest,
    response: &loomabase::replica::PartialReplicaResponse,
) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    apply_partial_replica_response(&tx, sent, response, &todos_table())?;
    tx.commit()?;
    Ok(())
}

fn incomplete_interest() -> ReplicaInterest {
    ReplicaInterest {
        predicates: vec![ReplicaPredicate::ColumnEquals {
            column: "completed".to_owned(),
            value: CrdtValue::Boolean(false),
        }],
        limit: 100,
    }
}

fn project_interest() -> ReplicaInterest {
    ReplicaInterest {
        predicates: vec![ReplicaPredicate::IdPrefix("project-a/".to_owned())],
        limit: 100,
    }
}

#[test]
fn authoritative_membership_adds_and_evicts_without_global_tombstones() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "partial-client")?;
    let mut server = CrdtState::default();
    server_write(
        &mut server,
        "writer",
        1,
        "project-a/1",
        BTreeMap::from([
            ("title".to_owned(), CrdtValue::Text("first".to_owned())),
            ("completed".to_owned(), CrdtValue::Boolean(false)),
            (DELETED_COLUMN.to_owned(), CrdtValue::Boolean(false)),
        ]),
    )?;

    let first = request(&mut connection, "incomplete", incomplete_interest())?;
    let first_response = server.merge_partial(first.clone(), "partial-client")?;
    apply(&mut connection, &first, &first_response)?;
    assert_eq!(
        read_todo(&connection, "project-a/1")?,
        Some(("first".to_owned(), false))
    );

    server_write(
        &mut server,
        "writer",
        2,
        "project-a/1",
        BTreeMap::from([("completed".to_owned(), CrdtValue::Boolean(true))]),
    )?;
    let second = request(&mut connection, "incomplete", incomplete_interest())?;
    let second_response = server.merge_partial(second.clone(), "partial-client")?;
    assert_eq!(second_response.evicted_row_ids, ["project-a/1"]);
    apply(&mut connection, &second, &second_response)?;

    assert_eq!(read_todo(&connection, "project-a/1")?, None);
    assert_eq!(
        server.cells[&("project-a/1".to_owned(), DELETED_COLUMN.to_owned())].value,
        CrdtValue::Boolean(false)
    );
    assert!(
        request(&mut connection, "incomplete", incomplete_interest())?
            .sync
            .changes
            .is_empty()
    );
    Ok(())
}

#[test]
fn overlapping_scope_membership_prevents_physical_eviction() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "partial-client")?;
    let mut server = CrdtState::default();
    server_write(
        &mut server,
        "writer",
        1,
        "project-a/1",
        BTreeMap::from([
            ("title".to_owned(), CrdtValue::Text("shared".to_owned())),
            ("completed".to_owned(), CrdtValue::Boolean(false)),
        ]),
    )?;
    for (scope, interest) in [
        ("incomplete", incomplete_interest()),
        ("project", project_interest()),
    ] {
        let sent = request(&mut connection, scope, interest)?;
        let response = server.merge_partial(sent.clone(), "partial-client")?;
        apply(&mut connection, &sent, &response)?;
    }

    server_write(
        &mut server,
        "writer",
        2,
        "project-a/1",
        BTreeMap::from([("completed".to_owned(), CrdtValue::Boolean(true))]),
    )?;
    let sent = request(&mut connection, "incomplete", incomplete_interest())?;
    let response = server.merge_partial(sent.clone(), "partial-client")?;
    apply(&mut connection, &sent, &response)?;

    // The row remains because the project scope still owns it. Refreshing that
    // scope then delivers the current server values.
    let project = request(&mut connection, "project", project_interest())?;
    let project_response = server.merge_partial(project.clone(), "partial-client")?;
    apply(&mut connection, &project, &project_response)?;
    assert_eq!(
        read_todo(&connection, "project-a/1")?,
        Some(("shared".to_owned(), true))
    );
    Ok(())
}

#[test]
fn concurrent_dirty_write_delays_eviction_until_a_later_ack() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "partial-client")?;
    let mut server = CrdtState::default();
    server_write(
        &mut server,
        "writer",
        1,
        "project-a/1",
        BTreeMap::from([
            ("title".to_owned(), CrdtValue::Text("initial".to_owned())),
            ("completed".to_owned(), CrdtValue::Boolean(false)),
        ]),
    )?;
    let initial = request(&mut connection, "incomplete", incomplete_interest())?;
    let initial_response = server.merge_partial(initial.clone(), "partial-client")?;
    apply(&mut connection, &initial, &initial_response)?;

    server_write(
        &mut server,
        "writer",
        2,
        "project-a/1",
        BTreeMap::from([("completed".to_owned(), CrdtValue::Boolean(true))]),
    )?;
    let in_flight = request(&mut connection, "incomplete", incomplete_interest())?;
    let in_flight_response = server.merge_partial(in_flight.clone(), "partial-client")?;
    connection.execute(
        "UPDATE todos SET title = ?2 WHERE id = ?1",
        params!["project-a/1", "offline while request was in flight"],
    )?;
    apply(&mut connection, &in_flight, &in_flight_response)?;
    assert_eq!(
        read_todo(&connection, "project-a/1")?,
        Some(("offline while request was in flight".to_owned(), false))
    );

    let retry = request(&mut connection, "incomplete", incomplete_interest())?;
    assert_eq!(retry.sync.changes.len(), 1);
    let retry_response = server.merge_partial(retry.clone(), "partial-client")?;
    apply(&mut connection, &retry, &retry_response)?;
    assert_eq!(read_todo(&connection, "project-a/1")?, None);
    assert_eq!(
        server.cells[&("project-a/1".to_owned(), "title".to_owned())].value,
        CrdtValue::Text("offline while request was in flight".to_owned())
    );
    Ok(())
}

#[test]
fn stale_out_of_order_scope_response_is_rejected_atomically() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "partial-client")?;
    let mut server = CrdtState::default();
    server_write(
        &mut server,
        "writer",
        1,
        "project-a/1",
        BTreeMap::from([("title".to_owned(), CrdtValue::Text("value".to_owned()))]),
    )?;
    let older = request(&mut connection, "project", project_interest())?;
    let older_response = server.merge_partial(older.clone(), "partial-client")?;
    let newer = request(&mut connection, "project", project_interest())?;
    assert!(newer.scope_version > older.scope_version);

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    assert!(apply_partial_replica_response(&tx, &older, &older_response, &todos_table()).is_err());
    tx.rollback()?;
    let membership: Option<String> = connection
        .query_row(
            "SELECT row_id FROM loomabase_scope_members WHERE scope_id = 'project'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    assert!(membership.is_none());
    Ok(())
}

#[test]
fn removing_a_scope_invalidates_in_flight_responses_and_evicts_members() -> Result<()> {
    let mut connection = Connection::open_in_memory()?;
    initialize_client(&mut connection, "partial-client")?;
    let mut server = CrdtState::default();
    server_write(
        &mut server,
        "writer",
        1,
        "project-a/1",
        BTreeMap::from([("title".to_owned(), CrdtValue::Text("value".to_owned()))]),
    )?;
    let initial = request(&mut connection, "project", project_interest())?;
    let initial_response = server.merge_partial(initial.clone(), "partial-client")?;
    apply(&mut connection, &initial, &initial_response)?;

    let in_flight = request(&mut connection, "project", project_interest())?;
    let in_flight_response = server.merge_partial(in_flight.clone(), "partial-client")?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    remove_partial_replica_scope(&tx, &todos_table(), "project")?;
    tx.commit()?;
    assert_eq!(read_todo(&connection, "project-a/1")?, None);

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    assert!(
        apply_partial_replica_response(&tx, &in_flight, &in_flight_response, &todos_table())
            .is_err()
    );
    tx.rollback()?;
    assert_eq!(
        server.cells[&("project-a/1".to_owned(), "title".to_owned())].value,
        CrdtValue::Text("value".to_owned())
    );
    Ok(())
}
