//! Deterministic, serializable explanations for every LWW decision.

use serde::{Deserialize, Serialize};

use crate::crdt::{ColumnMetadata, CrdtColumn, MergeDecision, decide_lww};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictWinner {
    Incoming,
    Current,
    Equal,
    InvalidConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    MissingCurrentValue,
    HigherLamportClock,
    LowerLamportClock,
    DeviceIdTieBreak,
    SameVersionSameValue,
    SameVersionDifferentValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictExplanation {
    pub winner: ConflictWinner,
    pub reason: ConflictReason,
    pub current: Option<ColumnMetadata>,
    pub incoming: ColumnMetadata,
    pub summary: String,
}

#[must_use]
pub fn explain_lww(current: Option<&CrdtColumn>, incoming: &CrdtColumn) -> ConflictExplanation {
    let Some(current) = current else {
        return ConflictExplanation {
            winner: ConflictWinner::Incoming,
            reason: ConflictReason::MissingCurrentValue,
            current: None,
            incoming: incoming.metadata.clone(),
            summary: "incoming value wins because the cell does not exist".to_owned(),
        };
    };

    let (winner, reason, summary) = match decide_lww(&current.metadata, &incoming.metadata) {
        MergeDecision::AcceptIncoming
            if incoming.metadata.lamport_clock > current.metadata.lamport_clock =>
        {
            (
                ConflictWinner::Incoming,
                ConflictReason::HigherLamportClock,
                format!(
                    "incoming clock {} is greater than current clock {}",
                    incoming.metadata.lamport_clock, current.metadata.lamport_clock
                ),
            )
        }
        MergeDecision::AcceptIncoming => (
            ConflictWinner::Incoming,
            ConflictReason::DeviceIdTieBreak,
            format!(
                "clocks are equal and incoming device ID {:?} sorts after current device ID {:?}",
                incoming.metadata.device_id, current.metadata.device_id
            ),
        ),
        MergeDecision::KeepCurrent
            if incoming.metadata.lamport_clock < current.metadata.lamport_clock =>
        {
            (
                ConflictWinner::Current,
                ConflictReason::LowerLamportClock,
                format!(
                    "incoming clock {} is lower than current clock {}",
                    incoming.metadata.lamport_clock, current.metadata.lamport_clock
                ),
            )
        }
        MergeDecision::KeepCurrent => (
            ConflictWinner::Current,
            ConflictReason::DeviceIdTieBreak,
            format!(
                "clocks are equal and incoming device ID {:?} sorts before current device ID {:?}",
                incoming.metadata.device_id, current.metadata.device_id
            ),
        ),
        MergeDecision::Equal if current.value == incoming.value => (
            ConflictWinner::Equal,
            ConflictReason::SameVersionSameValue,
            "incoming value is an idempotent replay of the current version".to_owned(),
        ),
        MergeDecision::Equal => (
            ConflictWinner::InvalidConflict,
            ConflictReason::SameVersionDifferentValue,
            "the same CRDT version identifies two different values".to_owned(),
        ),
    };

    ConflictExplanation {
        winner,
        reason,
        current: Some(current.metadata.clone()),
        incoming: incoming.metadata.clone(),
        summary,
    }
}
