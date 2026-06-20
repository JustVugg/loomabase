use std::collections::BTreeMap;

use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;
use loomabase::simulator::NetworkSimulator;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let table = todos_table();
    let payload = SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: table.fingerprint(),
        source_device_id: "device-a".into(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: "todo-1".into(),
            columns: BTreeMap::from([(
                "title".into(),
                CrdtColumn {
                    value: CrdtValue::Text("offline edit".into()),
                    metadata: ColumnMetadata {
                        lamport_clock: 1,
                        device_id: "device-a".into(),
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
    let mut simulator = NetworkSimulator::new(table);
    simulator.drop_payload(&payload, "device-a");
    simulator.duplicate(payload, "device-a")?;
    let output = "target/loomabase-simulation.html";
    std::fs::write(output, simulator.render_html())?;
    println!("Wrote visual simulation report to {output}");
    Ok(())
}
