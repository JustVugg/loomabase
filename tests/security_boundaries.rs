use std::collections::BTreeMap;
use std::sync::Arc;

use loomabase::crdt::{
    ColumnMetadata, CrdtColumn, CrdtState, CrdtValue, MAX_CLOCK_ADVANCE_PER_SYNC,
    MAX_STORABLE_LAMPORT, MAX_TEXT_BYTES, PROTOCOL_VERSION, RowChange, SERVER_DEVICE_ID,
    SyncPayload, SyncRejectionKind, validate_column,
};
use loomabase::policy::{
    AllowAllAuthorizer, ColumnAllowListAuthorizer, MaxTextLengthValidator, NoopValidator,
    SyncSecurity,
};
use loomabase::schema::{ColumnDef, ColumnType, TableDef, todos_table};

fn payload(value: CrdtValue) -> SyncPayload {
    SyncPayload {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: todos_table().fingerprint(),
        source_device_id: "device-a".to_owned(),
        source_lamport: 1,
        changes: vec![RowChange {
            todo_id: "todo-1".to_owned(),
            columns: BTreeMap::from([(
                "title".to_owned(),
                CrdtColumn {
                    value,
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
fn rejects_unknown_columns_and_wrong_types() {
    let mut unknown = payload(CrdtValue::Text("valid".to_owned()));
    let column = unknown.changes[0].columns.remove("title").unwrap();
    unknown.changes[0]
        .columns
        .insert("is_admin".to_owned(), column);
    assert!(unknown.validate(&todos_table()).is_err());

    assert!(
        payload(CrdtValue::Boolean(true))
            .validate(&todos_table())
            .is_err()
    );
}

#[test]
fn rejects_resource_exhaustion_and_unstorable_clocks() {
    let oversized_title = "x".repeat(MAX_TEXT_BYTES + 1);
    assert!(
        payload(CrdtValue::Text(oversized_title))
            .validate(&todos_table())
            .is_err()
    );

    let mut overflow = payload(CrdtValue::Text("valid".to_owned()));
    overflow.source_lamport = MAX_STORABLE_LAMPORT + 1;
    overflow.changes[0]
        .columns
        .get_mut("title")
        .unwrap()
        .metadata
        .lamport_clock = MAX_STORABLE_LAMPORT + 1;
    assert!(overflow.validate(&todos_table()).is_err());
}

#[test]
fn rejects_duplicate_rows_and_control_characters() {
    let mut duplicate = payload(CrdtValue::Text("valid".to_owned()));
    duplicate.changes.push(duplicate.changes[0].clone());
    assert!(duplicate.validate(&todos_table()).is_err());

    let mut control = payload(CrdtValue::Text("valid".to_owned()));
    control.changes[0].todo_id = "todo\n1".to_owned();
    assert!(control.validate(&todos_table()).is_err());
}

#[test]
fn rejected_spoofing_cannot_mutate_state() {
    let mut state = CrdtState::default();
    let before = state.clone();
    assert!(
        state
            .merge(payload(CrdtValue::Text("attack".to_owned())), "device-b")
            .is_err()
    );
    assert_eq!(state, before);
}

#[test]
fn rejects_clock_jump_dos_and_reserved_server_identity() {
    let mut state = CrdtState::default();
    let mut jump = payload(CrdtValue::Text("attack".to_owned()));
    jump.source_lamport = MAX_CLOCK_ADVANCE_PER_SYNC + 1;
    jump.changes[0]
        .columns
        .get_mut("title")
        .unwrap()
        .metadata
        .lamport_clock = MAX_CLOCK_ADVANCE_PER_SYNC + 1;
    assert!(state.merge(jump, "device-a").is_err());
    assert_eq!(state, CrdtState::default());

    let mut reserved = payload(CrdtValue::Text("attack".to_owned()));
    reserved.source_device_id = SERVER_DEVICE_ID.to_owned();
    reserved.changes[0]
        .columns
        .get_mut("title")
        .unwrap()
        .metadata
        .device_id = SERVER_DEVICE_ID.to_owned();
    assert!(state.merge(reserved, SERVER_DEVICE_ID).is_err());
}

#[test]
fn authorization_denials_are_reported_without_mutating_state() {
    let table = todos_table();
    let security = SyncSecurity::without_audit(
        Arc::new(ColumnAllowListAuthorizer::new(&table, ["completed"]).unwrap()),
        Arc::new(NoopValidator),
    );
    let mut state = CrdtState::default();

    let response = state
        .merge_with_security(
            payload(CrdtValue::Text("not allowed".to_owned())),
            "device-a",
            &security,
        )
        .unwrap();

    assert!(state.cells.is_empty());
    assert_eq!(response.rejections.len(), 1);
    assert_eq!(
        response.rejections[0].kind,
        SyncRejectionKind::AuthorizationDenied
    );
    assert_eq!(response.rejections[0].column_name, "title");
}

#[test]
fn business_validation_denials_are_reported_without_mutating_state() {
    let security = SyncSecurity::without_audit(
        Arc::new(AllowAllAuthorizer),
        Arc::new(MaxTextLengthValidator::all_text_columns(3)),
    );
    let mut state = CrdtState::default();

    let response = state
        .merge_with_security(
            payload(CrdtValue::Text("too long".to_owned())),
            "device-a",
            &security,
        )
        .unwrap();

    assert!(state.cells.is_empty());
    assert_eq!(response.rejections.len(), 1);
    assert_eq!(
        response.rejections[0].kind,
        SyncRejectionKind::ValidationFailed
    );
    assert!(response.rejections[0].reason.contains("exceeds 3 bytes"));
}

#[test]
fn payload_json_round_trip_preserves_types_and_versions() {
    let original = payload(CrdtValue::Text("typed value".to_owned()));
    let encoded = serde_json::to_vec(&original).unwrap();
    let decoded: SyncPayload = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn rejects_non_server_remote_payloads() {
    let client_payload = payload(CrdtValue::Text("valid".to_owned()));
    assert!(
        client_payload
            .validate_server_response(&todos_table())
            .is_err()
    );
}

#[test]
fn rejects_unknown_protocol_versions_before_mutation() {
    let mut state = CrdtState::default();
    let mut incompatible = payload(CrdtValue::Text("future".to_owned()));
    incompatible.protocol_version = PROTOCOL_VERSION + 1;
    assert!(state.merge(incompatible, "device-a").is_err());
    assert_eq!(state, CrdtState::default());
}

#[test]
fn accepts_the_previous_protocol_during_rolling_upgrades() {
    let mut previous = payload(CrdtValue::Text("compatible".to_owned()));
    previous.protocol_version = PROTOCOL_VERSION - 1;
    assert!(previous.validate(&todos_table()).is_ok());

    let json = format!(
        r#"{{"protocol_version":{},"schema_fingerprint":{},"source_device_id":"device-a","source_lamport":0,"changes":[],"cursor":0}}"#,
        PROTOCOL_VERSION - 1,
        todos_table().fingerprint()
    );
    let decoded: SyncPayload = serde_json::from_str(&json).unwrap();
    assert!(!decoded.has_more);
    assert!(!decoded.cursor_reset);
    assert!(decoded.cursor_token.is_none());
    assert!(decoded.server_epoch.is_none());

    let mut state = CrdtState::default();
    let response = state.merge(decoded, "device-a").unwrap();
    assert_eq!(response.protocol_version, PROTOCOL_VERSION - 1);
}

#[test]
fn rejects_non_finite_reals_and_invalid_request_cursor_flags() {
    let metrics =
        TableDef::new("metrics", vec![ColumnDef::new("score", ColumnType::Real)]).unwrap();
    assert!(validate_column(&metrics, "score", &CrdtValue::Real(f64::NAN)).is_err());
    assert!(validate_column(&metrics, "score", &CrdtValue::Real(f64::INFINITY)).is_err());

    let mut invalid_cursor = payload(CrdtValue::Text("valid".to_owned()));
    invalid_cursor.cursor = -1;
    assert!(invalid_cursor.validate(&todos_table()).is_err());

    let mut response_flags = payload(CrdtValue::Text("valid".to_owned()));
    response_flags.has_more = true;
    assert!(
        response_flags
            .validate_client_request("device-a", &todos_table())
            .is_err()
    );
}
