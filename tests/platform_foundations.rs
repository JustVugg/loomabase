use std::collections::BTreeMap;

use loomabase::codegen::{SdkLanguage, generate_sdk};
use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtValue, PROTOCOL_VERSION, RowChange, SyncPayload,
};
use loomabase::explain::{ConflictReason, ConflictWinner, explain_lww};
use loomabase::replica::{ReplicaInterest, ReplicaPredicate};
use loomabase::schema::{Contract, todos_table};
use loomabase::simulator::NetworkSimulator;

fn title_column(clock: u64, device: &str, value: &str) -> CrdtColumn {
    CrdtColumn {
        value: CrdtValue::Text(value.to_owned()),
        metadata: ColumnMetadata {
            lamport_clock: clock,
            device_id: device.to_owned(),
        },
    }
}

fn title_payload(clock: u64, device: &str, value: &str) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: device.to_owned(),
        source_lamport: clock,
        changes: vec![RowChange {
            todo_id: "todo-1".to_owned(),
            columns: BTreeMap::from([("title".to_owned(), title_column(clock, device, value))]),
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
fn sdk_generator_emits_four_typed_targets_with_contract_fingerprint() {
    let contract = Contract::new(vec![todos_table()]).unwrap();
    for language in [
        SdkLanguage::Swift,
        SdkLanguage::Kotlin,
        SdkLanguage::TypeScript,
        SdkLanguage::Dart,
    ] {
        let generated = generate_sdk(&contract, language);
        assert_eq!(generated.files.len(), 2);
        let joined = generated.files.values().cloned().collect::<String>();
        assert!(joined.contains("Todo"));
        assert!(joined.contains(&contract.fingerprint().to_string()));
    }
}

#[test]
fn conflict_explanation_identifies_clock_and_device_tie_breaks() {
    let current = title_column(7, "device-a", "A");
    let higher = explain_lww(Some(&current), &title_column(8, "device-b", "B"));
    assert_eq!(higher.winner, ConflictWinner::Incoming);
    assert_eq!(higher.reason, ConflictReason::HigherLamportClock);

    let tied = explain_lww(Some(&current), &title_column(7, "device-z", "Z"));
    assert_eq!(tied.winner, ConflictWinner::Incoming);
    assert_eq!(tied.reason, ConflictReason::DeviceIdTieBreak);
}

#[test]
fn replica_interest_generates_parameterized_plan_and_matches_rows() {
    let interest = ReplicaInterest {
        predicates: vec![
            ReplicaPredicate::IdPrefix("project-a/".to_owned()),
            ReplicaPredicate::ColumnEquals {
                column: "completed".to_owned(),
                value: CrdtValue::Boolean(false),
            },
        ],
        limit: 500,
    };
    let plan = interest.postgres_plan(&todos_table(), "tenant-a").unwrap();
    assert!(plan.sql.contains("starts_with(id, $2)"));
    assert!(plan.sql.contains("completed = $3"));
    assert!(!plan.sql.contains("project-a/"));
    assert!(interest.matches(
        "project-a/todo-1",
        &BTreeMap::from([("completed".to_owned(), CrdtValue::Boolean(false))])
    ));
}

#[test]
fn simulator_records_failures_duplicates_and_renders_visual_report() {
    let payload = title_payload(1, "device-a", "<offline>");
    let mut simulator = NetworkSimulator::new(todos_table());
    simulator.drop_payload(&payload, "device-a");
    simulator.duplicate(payload, "device-a").unwrap();
    let html = simulator.render_html();
    assert_eq!(simulator.trace().len(), 3);
    assert!(html.contains("Loomabase deterministic sync simulation"));
    assert!(html.contains("&lt;offline&gt;") || html.contains("Final cells: 1"));
}
