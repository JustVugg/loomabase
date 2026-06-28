use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtState, CrdtValue, MAX_RESPONSE_CELLS, PROTOCOL_VERSION,
    RowChange, SyncPayload,
};
use loomabase::schema::todos_table;

fn title_payload(device_id: &str, clock: u64, title: &str) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.to_owned(),
        source_lamport: clock,
        changes: vec![RowChange {
            todo_id: "todo-1".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text(title.to_owned()),
                    metadata: ColumnMetadata {
                        lamport_clock: clock,
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

#[test]
fn merge_is_commutative_for_cells() -> Result<()> {
    let a = title_payload("device-a", 8, "A");
    let b = title_payload("device-b", 8, "B");
    let mut left = CrdtState::default();
    let mut right = CrdtState::default();

    left.merge(a.clone(), "device-a")?;
    left.merge(b.clone(), "device-b")?;
    right.merge(b, "device-b")?;
    right.merge(a, "device-a")?;

    assert_eq!(left.cells, right.cells);
    Ok(())
}

#[test]
fn merge_is_associative_across_all_delivery_orders() -> Result<()> {
    let payloads = [
        title_payload("device-a", 3, "A"),
        title_payload("device-b", 9, "B"),
        title_payload("device-c", 5, "C"),
    ];
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut expected = None;

    for order in orders {
        let mut state = CrdtState::default();
        for index in order {
            let payload = payloads[index].clone();
            let device_id = payload.source_device_id.clone();
            state.merge(payload, &device_id)?;
        }
        match &expected {
            None => expected = Some(state.cells),
            Some(cells) => assert_eq!(&state.cells, cells),
        }
    }
    Ok(())
}

#[test]
fn failed_merge_is_atomic() -> Result<()> {
    let mut state = CrdtState::default();
    state.merge(title_payload("device-a", 5, "current"), "device-a")?;
    let before = state.clone();

    let mut conflict = title_payload("device-a", 5, "different value");
    conflict.changes.insert(
        0,
        RowChange {
            todo_id: "another-todo".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text("must roll back".to_owned()),
                    metadata: ColumnMetadata {
                        lamport_clock: 5,
                        device_id: "device-a".to_owned(),
                    },
                },
            )]),
        },
    );

    assert!(state.merge(conflict, "device-a").is_err());
    assert_eq!(state, before);
    Ok(())
}

#[test]
fn change_feed_is_bounded_and_cursors_are_device_bound() -> Result<()> {
    let mut state = CrdtState::default();
    let changes = (0..=MAX_RESPONSE_CELLS)
        .map(|index| RowChange {
            todo_id: format!("todo-{index:04}"),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text(format!("title-{index}")),
                    metadata: ColumnMetadata {
                        lamport_clock: 1,
                        device_id: "device-a".to_owned(),
                    },
                },
            )]),
        })
        .collect();
    let push = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "device-a".to_owned(),
        source_lamport: 1,
        changes,
        cursor: 0,
        has_more: false,
        cursor_reset: false,
        cursor_token: None,
        server_epoch: None,
        rejections: Vec::new(),
    };

    let first = state.merge(push, "device-a")?;
    assert_eq!(first.changes.len(), MAX_RESPONSE_CELLS);
    assert!(first.has_more);

    let mut next = SyncPayload::empty("device-a", first.source_lamport, &todos_table());
    next.cursor = first.cursor;
    let second = state.merge(next, "device-a")?;
    assert_eq!(second.changes.len(), 1);
    assert!(!second.has_more);

    let mut forged = SyncPayload::empty("device-b", 1, &todos_table());
    forged.cursor = second.cursor;
    let repaired = state.merge(forged, "device-b")?;
    assert!(repaired.cursor_reset);
    assert_eq!(repaired.changes.len(), MAX_RESPONSE_CELLS);
    Ok(())
}
