#![no_main]

use libfuzzer_sys::fuzz_target;
use loomabase::crdt::CrdtState;
use loomabase::replica::{PartialReplicaRequest, PartialReplicaResponse};
use loomabase::schema::todos_table;

fuzz_target!(|data: &[u8]| {
    if let Ok(request) = serde_json::from_slice::<PartialReplicaRequest>(data) {
        let device_id = request.sync.source_device_id.clone();
        let _ = request.validate(&todos_table(), &device_id);
        let _ = CrdtState::default().merge_partial(request, &device_id);
    }
    if let Ok(response) = serde_json::from_slice::<PartialReplicaResponse>(data) {
        let _ = response.validate(&todos_table());
    }
});
