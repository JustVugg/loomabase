//! Deterministic network simulation with a self-contained visual HTML report.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::crdt::{CrdtState, SyncPayload};
use crate::error::Result;
use crate::schema::TableDef;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationAction {
    Delivered,
    Dropped,
    Duplicated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SimulationEvent {
    pub step: u64,
    pub device_id: String,
    pub action: SimulationAction,
    pub changes: usize,
    pub result: String,
}

#[derive(Clone, Debug)]
pub struct NetworkSimulator {
    server: CrdtState,
    trace: Vec<SimulationEvent>,
    step: u64,
}

impl NetworkSimulator {
    #[must_use]
    pub fn new(table: TableDef) -> Self {
        Self {
            server: CrdtState::new(table),
            trace: Vec::new(),
            step: 0,
        }
    }

    pub fn deliver(&mut self, payload: SyncPayload, device_id: &str) -> Result<SyncPayload> {
        self.step += 1;
        let changes = payload.changes.len();
        let response = self.server.merge(payload, device_id)?;
        self.trace.push(SimulationEvent {
            step: self.step,
            device_id: device_id.to_owned(),
            action: SimulationAction::Delivered,
            changes,
            result: format!("accepted; {} response rows", response.changes.len()),
        });
        Ok(response)
    }

    pub fn duplicate(&mut self, payload: SyncPayload, device_id: &str) -> Result<SyncPayload> {
        let first = self.deliver(payload.clone(), device_id)?;
        self.step += 1;
        let second = self.server.merge(payload, device_id)?;
        self.trace.push(SimulationEvent {
            step: self.step,
            device_id: device_id.to_owned(),
            action: SimulationAction::Duplicated,
            changes: second.changes.len(),
            result: "duplicate delivery preserved cell state".to_owned(),
        });
        Ok(first)
    }

    pub fn drop_payload(&mut self, payload: &SyncPayload, device_id: &str) {
        self.step += 1;
        self.trace.push(SimulationEvent {
            step: self.step,
            device_id: device_id.to_owned(),
            action: SimulationAction::Dropped,
            changes: payload.changes.len(),
            result: "network dropped payload before merge".to_owned(),
        });
    }

    #[must_use]
    pub fn trace(&self) -> &[SimulationEvent] {
        &self.trace
    }

    #[must_use]
    pub fn cells(&self) -> &std::collections::BTreeMap<(String, String), crate::crdt::CrdtColumn> {
        &self.server.cells
    }

    #[must_use]
    pub fn render_html(&self) -> String {
        let mut rows = String::new();
        for event in &self.trace {
            let _ = write!(
                rows,
                "<tr><td>{}</td><td>{}</td><td>{:?}</td><td>{}</td><td>{}</td></tr>",
                event.step,
                escape_html(&event.device_id),
                event.action,
                event.changes,
                escape_html(&event.result)
            );
        }
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Loomabase Sync Simulation</title>\
             <style>body{{font-family:system-ui;margin:2rem;background:#0b1020;color:#e8ecff}}\
             table{{border-collapse:collapse;width:100%}}th,td{{padding:.7rem;border:1px solid #34405f}}\
             th{{background:#18213b}}h1{{color:#8de1ff}}</style></head><body>\
             <h1>Loomabase deterministic sync simulation</h1><p>Final cells: {}</p>\
             <table><thead><tr><th>Step</th><th>Device</th><th>Action</th><th>Rows</th><th>Result</th></tr></thead>\
             <tbody>{rows}</tbody></table></body></html>",
            self.server.cells.len()
        )
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
