use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::client::SqliteClient;
use loomabase::crdt::CrdtValue;
use loomabase::schema::{ColumnDef, ColumnType, TableDef};

fn notes_v1() -> TableDef {
    TableDef::new("notes", vec![ColumnDef::new("body", ColumnType::Text)]).unwrap()
}

fn notes_v2() -> TableDef {
    TableDef::new(
        "notes",
        vec![
            ColumnDef::new("body", ColumnType::Text),
            ColumnDef::new("priority", ColumnType::Integer),
        ],
    )
    .unwrap()
}

fn temp_db(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("loomabase-{tag}-{}.db", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    path
}

#[tokio::test]
async fn additive_migration_preserves_data_and_enables_new_column() -> Result<()> {
    let path = temp_db("migrate-additive");

    // V1: a row with only `body`.
    {
        let client = SqliteClient::open_with(&path, "device-a", notes_v1()).await?;
        client
            .insert(
                "n1".into(),
                BTreeMap::from([("body".into(), CrdtValue::Text("hello".into()))]),
            )
            .await?;
    }

    // Reopen with V2: the additive migration runs on the existing database.
    {
        let client = SqliteClient::open_with(&path, "device-a", notes_v2()).await?;

        // Pre-existing synchronized data survives the migration.
        assert_eq!(
            client.get_cell("n1".into(), "body".into()).await?,
            Some(CrdtValue::Text("hello".into()))
        );
        // The migrated column has no CRDT version on the old row until written.
        assert_eq!(client.get_cell("n1".into(), "priority".into()).await?, None);

        // It is writable and then carries a version.
        client
            .set("n1".into(), "priority".into(), CrdtValue::Integer(7))
            .await?;
        assert_eq!(
            client.get_cell("n1".into(), "priority".into()).await?,
            Some(CrdtValue::Integer(7))
        );

        // Rows inserted after the migration capture the new column directly.
        client
            .insert(
                "n2".into(),
                BTreeMap::from([
                    ("body".into(), CrdtValue::Text("second".into())),
                    ("priority".into(), CrdtValue::Integer(3)),
                ]),
            )
            .await?;
        assert_eq!(
            client.get_cell("n2".into(), "priority".into()).await?,
            Some(CrdtValue::Integer(3))
        );
    }

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    Ok(())
}

#[tokio::test]
async fn destructive_migration_is_rejected() -> Result<()> {
    let path = temp_db("migrate-destructive");

    {
        let client = SqliteClient::open_with(&path, "device-a", notes_v1()).await?;
        client
            .insert(
                "n1".into(),
                BTreeMap::from([("body".into(), CrdtValue::Text("hello".into()))]),
            )
            .await?;
    }

    // Reopening with `body` retyped from Text to Integer is destructive.
    let incompatible =
        TableDef::new("notes", vec![ColumnDef::new("body", ColumnType::Integer)]).unwrap();
    assert!(
        SqliteClient::open_with(&path, "device-a", incompatible)
            .await
            .is_err()
    );

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    Ok(())
}

#[tokio::test]
async fn removed_column_migration_is_rejected() -> Result<()> {
    let path = temp_db("migrate-removed");

    {
        let client = SqliteClient::open_with(&path, "device-a", notes_v2()).await?;
        client
            .insert(
                "n1".into(),
                BTreeMap::from([
                    ("body".into(), CrdtValue::Text("hello".into())),
                    ("priority".into(), CrdtValue::Integer(5)),
                ]),
            )
            .await?;
    }

    assert!(
        SqliteClient::open_with(&path, "device-a", notes_v1())
            .await
            .is_err()
    );

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    Ok(())
}
