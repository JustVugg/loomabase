use loomabase::Result;
use loomabase::client::{
    acknowledge_local_delta, apply_remote_payload, get_local_delta,
    initialize_client_with_contract, read_todo,
};
use loomabase::crdt::CrdtState;
use loomabase::schema::{ColumnDef, ColumnType, Contract, TableDef, todos_table};
use rusqlite::{Connection, TransactionBehavior, params};

fn notes_table() -> TableDef {
    TableDef::new("notes", vec![ColumnDef::new("body", ColumnType::Text)]).unwrap()
}

/// One sync round for a single table of the contract against its reference server.
fn sync_table(
    connection: &mut Connection,
    table: &TableDef,
    server: &mut CrdtState,
    device_id: &str,
) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let delta = get_local_delta(&tx, table)?;
    tx.commit()?;

    let response = server.merge(delta.clone(), device_id)?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    acknowledge_local_delta(&tx, &delta, table)?;
    apply_remote_payload(&tx, &response, table)?;
    tx.commit()?;
    Ok(())
}

fn note_body(connection: &Connection, id: &str) -> Option<String> {
    connection
        .query_row(
            "SELECT body FROM notes WHERE id = ?1 AND deleted = 0",
            [id],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

#[test]
fn multi_table_contract_syncs_each_table_independently() -> Result<()> {
    let todos = todos_table();
    let notes = notes_table();
    let contract = Contract::new(vec![todos.clone(), notes.clone()])?;

    let mut device_a = Connection::open_in_memory()?;
    let mut device_b = Connection::open_in_memory()?;
    initialize_client_with_contract(&mut device_a, "device-a", &contract)?;
    initialize_client_with_contract(&mut device_b, "device-b", &contract)?;

    let mut todos_server = CrdtState::new(todos.clone());
    let mut notes_server = CrdtState::new(notes.clone());

    // Device A writes to BOTH tables in the one edge database.
    device_a.execute(
        "INSERT INTO todos(id, title, completed) VALUES (?1, ?2, ?3)",
        params!["t1", "a todo", false],
    )?;
    device_a.execute(
        "INSERT INTO notes(id, body) VALUES (?1, ?2)",
        params!["n1", "a note"],
    )?;

    // Each table synchronizes through its own per-table flow.
    sync_table(&mut device_a, &todos, &mut todos_server, "device-a")?;
    sync_table(&mut device_a, &notes, &mut notes_server, "device-a")?;

    assert!(
        todos_server
            .cells
            .contains_key(&("t1".to_owned(), "title".to_owned()))
    );
    assert!(
        notes_server
            .cells
            .contains_key(&("n1".to_owned(), "body".to_owned()))
    );

    // Device B pulls both tables and converges on the same edge database.
    sync_table(&mut device_b, &todos, &mut todos_server, "device-b")?;
    sync_table(&mut device_b, &notes, &mut notes_server, "device-b")?;

    assert_eq!(
        read_todo(&device_b, "t1")?,
        Some(("a todo".to_owned(), false))
    );
    assert_eq!(note_body(&device_b, "n1"), Some("a note".to_owned()));
    Ok(())
}

#[test]
fn contract_fingerprint_distinguishes_table_sets() {
    let a = Contract::new(vec![todos_table()]).unwrap();
    let b = Contract::new(vec![todos_table(), notes_table()]).unwrap();
    assert_ne!(a.fingerprint(), b.fingerprint());
    // Order-independent.
    let c = Contract::new(vec![notes_table(), todos_table()]).unwrap();
    assert_eq!(b.fingerprint(), c.fingerprint());
}

#[test]
fn contract_rejects_empty_and_duplicate_tables() {
    assert!(Contract::new(vec![]).is_err());
    assert!(Contract::new(vec![todos_table(), todos_table()]).is_err());
}
