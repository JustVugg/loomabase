#![no_main]

use libfuzzer_sys::fuzz_target;
use loomabase::crdt::{CrdtState, SyncPayload};
use loomabase::schema::todos_table;

fuzz_target!(|data: &[u8]| {
    let Ok(payload) = serde_json::from_slice::<SyncPayload>(data) else {
        return;
    };
    let device_id = payload.source_device_id.clone();
    let _ = payload.validate(&todos_table());
    let _ = CrdtState::default().merge(payload, &device_id);
});

