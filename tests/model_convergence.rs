use std::collections::BTreeMap;

use loomabase::Result;
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtState, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;

fn operation(index: u64) -> SyncPayload {
    let device_id = format!("device-{}", index % 7);
    let column = if index.is_multiple_of(3) {
        (
            "completed".to_owned(),
            CrdtValue::Boolean(index.is_multiple_of(2)),
        )
    } else {
        (
            "title".to_owned(),
            CrdtValue::Text(format!("value-{index}")),
        )
    };
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.clone(),
        source_lamport: index + 1,
        changes: vec![RowChange {
            todo_id: format!("todo-{}", index % 11),
            columns: BTreeMap::from([(
                column.0,
                CrdtColumn {
                    value: column.1,
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

fn shuffle<T>(values: &mut [T], mut state: u64) {
    for index in (1..values.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let upper_bound = u64::try_from(index).unwrap() + 1;
        let other = usize::try_from(state % upper_bound).unwrap();
        values.swap(index, other);
    }
}

#[test]
fn randomized_reordering_and_duplicate_delivery_converge() -> Result<()> {
    let operations = (0..256).map(operation).collect::<Vec<_>>();
    let mut canonical = CrdtState::default();
    for payload in &operations {
        canonical.merge(payload.clone(), &payload.source_device_id)?;
    }

    for seed in 0..32 {
        let mut schedule = operations.clone();
        schedule.extend(operations.iter().step_by(5).cloned());
        shuffle(&mut schedule, seed);

        let mut replica = CrdtState::default();
        for payload in schedule {
            let device_id = payload.source_device_id.clone();
            replica.merge(payload, &device_id)?;
        }
        assert_eq!(replica.cells, canonical.cells, "failed seed {seed}");
    }
    Ok(())
}
