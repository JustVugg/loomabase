use std::collections::BTreeMap;
use std::time::Instant;

use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtState, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;

fn payload(index: u64) -> SyncPayload {
    let device_id = format!("bench-device-{}", index % 32);
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device_id.clone(),
        source_lamport: index + 1,
        changes: vec![RowChange {
            todo_id: format!("row-{}", index % 10_000),
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

fn main() -> loomabase::Result<()> {
    let operations = std::env::var("LOOMABASE_BENCH_OPERATIONS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(100_000);
    let started = Instant::now();
    let mut state = CrdtState::default();
    for index in 0..operations {
        let payload = payload(u64::from(index));
        let device_id = payload.source_device_id.clone();
        state.merge(payload, &device_id)?;
    }
    let elapsed = started.elapsed();
    let ops_per_second = f64::from(operations) / elapsed.as_secs_f64();
    println!(
        "{{\"engine\":\"loomabase-reference\",\"operations\":{operations},\"seconds\":{:.6},\"ops_per_second\":{ops_per_second:.2},\"final_cells\":{}}}",
        elapsed.as_secs_f64(),
        state.cells.len()
    );
    Ok(())
}
