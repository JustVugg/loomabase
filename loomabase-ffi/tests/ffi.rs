use std::collections::BTreeMap;
use std::ffi::{CStr, CString};

use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::schema::todos_table;
use loomabase_ffi::{
    LOOMABASE_ABI_VERSION, loomabase_abi_version, loomabase_last_error_message,
    loomabase_state_free, loomabase_state_merge, loomabase_state_new, loomabase_string_free,
};

fn title_payload() -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "device-a".to_owned(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: "t1".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value: CrdtValue::Text("from ffi".to_owned()),
                    metadata: ColumnMetadata {
                        lamport_clock: 1,
                        device_id: "device-a".to_owned(),
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
fn ffi_merge_round_trips_through_the_c_abi() {
    let state = loomabase_state_new();
    assert!(!state.is_null());

    // Device A pushes a todo through the C ABI.
    let push = CString::new(serde_json::to_string(&title_payload()).unwrap()).unwrap();
    let device_a = CString::new("device-a").unwrap();
    let pushed = unsafe { loomabase_state_merge(state, push.as_ptr(), device_a.as_ptr()) };
    assert!(!pushed.is_null());
    unsafe { loomabase_string_free(pushed) };

    // Device B pulls (empty payload) and receives that todo.
    let pull = SyncPayload::empty("device-b", 5, &todos_table());
    let pull = CString::new(serde_json::to_string(&pull).unwrap()).unwrap();
    let device_b = CString::new("device-b").unwrap();
    let response_ptr = unsafe { loomabase_state_merge(state, pull.as_ptr(), device_b.as_ptr()) };
    assert!(!response_ptr.is_null());

    let response_json = unsafe { CStr::from_ptr(response_ptr) }.to_str().unwrap();
    let response: SyncPayload = serde_json::from_str(response_json).unwrap();
    assert!(response.changes.iter().any(|row| row.todo_id == "t1"));

    unsafe { loomabase_string_free(response_ptr) };
    unsafe { loomabase_state_free(state) };
}

#[test]
fn ffi_returns_null_for_invalid_payload() {
    let state = loomabase_state_new();
    let bad = CString::new("{ not valid json").unwrap();
    let device = CString::new("device-a").unwrap();

    let response_ptr = unsafe { loomabase_state_merge(state, bad.as_ptr(), device.as_ptr()) };
    assert!(response_ptr.is_null());
    let error = loomabase_last_error_message();
    assert!(!error.is_null());
    let error = unsafe { CStr::from_ptr(error) }.to_str().unwrap();
    assert!(error.contains("invalid payload JSON"));

    unsafe { loomabase_state_free(state) };
}

#[test]
fn ffi_exposes_its_abi_version() {
    assert_eq!(loomabase_abi_version(), LOOMABASE_ABI_VERSION);
}

#[test]
fn ffi_state_serializes_concurrent_merges() {
    let state = loomabase_state_new() as usize;
    let threads = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                let state = state as *mut loomabase_ffi::LoomabaseState;
                let payload =
                    CString::new(serde_json::to_string(&title_payload()).unwrap()).unwrap();
                let device = CString::new("device-a").unwrap();
                let response =
                    unsafe { loomabase_state_merge(state, payload.as_ptr(), device.as_ptr()) };
                assert!(!response.is_null());
                unsafe { loomabase_string_free(response) };
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    unsafe { loomabase_state_free(state as *mut loomabase_ffi::LoomabaseState) };
}
