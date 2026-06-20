use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtState, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;

fn operation(index: u64) -> SyncPayload {
    let device_id = format!("device-{}", index % 32);
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.clone(),
        source_lamport: index + 1,
        changes: vec![RowChange {
            todo_id: format!("todo-{}", index % 2_000),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text(format!("value-{index}")),
                    metadata: ColumnMetadata {
                        lamport_clock: index + 1,
                        device_id,
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
#[ignore = "scheduled soak test"]
fn repeated_model_runs_remain_deterministic() -> Result<()> {
    let operations = (0..25_000).map(operation).collect::<Vec<_>>();
    let mut forward = CrdtState::default();
    for payload in &operations {
        forward.merge(payload.clone(), &payload.source_device_id)?;
    }
    let mut reverse = CrdtState::default();
    for payload in operations.iter().rev() {
        reverse.merge(payload.clone(), &payload.source_device_id)?;
        reverse.merge(payload.clone(), &payload.source_device_id)?;
    }
    assert_eq!(forward.cells, reverse.cells);
    Ok(())
}
