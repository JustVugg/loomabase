# Loomabase Product and Technical Vision

Loomabase should not differentiate through a new transport wrapper around
existing replication. Its durable advantage should be making offline-first
systems easier to specify, prove, inspect, and operate.

## Current Differentiators

- Typed, column-level CRDT registers preserve unrelated offline edits.
- Version-aware acknowledgement closes a common extraction/acknowledgement
  race that can silently lose newer local writes.
- Device identity is bound to every submitted mutation, and untrusted clocks
  are bounded before they can exhaust the server clock.
- The merge core is transport-independent and has an in-memory reference model
  that can be used as an executable specification.
- `sync_until_caught_up` makes bounded, transactional pagination the easiest
  API.

## Sync Contract Compiler

The current `TableDef` contract already generates validated SQLite/PostgreSQL
schema, triggers, migrations, fingerprints, and generic row APIs. The next
major capability is a richer declarative contract and generated typed SDKs:

```text
table todos {
  id: text primary_key
  title: text conflict = lww
  completed: boolean conflict = lww
}
```

From one reviewed contract, Loomabase should generate:

- typed Rust payload and row APIs;
- SQLite tables, metadata, triggers, and migrations;
- PostgreSQL metadata, merge statements, indexes, and RLS policy hooks;
- a stable schema fingerprint embedded in every sync handshake;
- compatibility diagnostics before an incompatible client can mutate data.

This removes hand-written trigger and merge SQL, prevents client/server schema
drift, and makes custom application schemas safe by construction.

## Capabilities That Could Make Loomabase Distinctive

### Conflict Explainability

Every accepted or rejected mutation should optionally produce a compact,
queryable decision record: which version won, why it won, which device wrote
it, and which policy authorized it. Operators and users should be able to
answer "why is this value here?" without reconstructing logs manually.

### Deterministic Sync Laboratory

Ship a simulation engine and conformance suite that permutes delivery order,
duplicates, disconnects, clock skew, transaction failures, and process
restarts. Generated adapters should have to pass convergence and atomicity
laws before release. This turns distributed-system testing into a normal
developer workflow.

### Adaptive Anti-Entropy

The current bounded feed uses opaque capabilities, server leases, and epochs
for normal incremental repair. Very large or partially replicated datasets
should additionally support stateless signed cursors, partitioned Merkle
summaries, and range repair. The protocol should choose the cheapest correct
reconciliation strategy without changing application code.

### Policy-Aware Offline Writes

Authorization should be part of the sync contract. Clients can evaluate a
portable subset locally for immediate UX, while the server remains
authoritative and returns structured rejections. Policy versions and schema
versions should travel together so an old offline client cannot unknowingly
write under obsolete rules.

### Partial Replicas as a First-Class Concept

Applications rarely need the whole database on every device. Loomabase should
continue expanding its implemented durable, parameterized replica interests.
The current protocol provides authoritative membership snapshots, explicit
local-only eviction, overlapping scopes, dirty-write preservation, and
out-of-order response protection. Future work can add more predicate operators
and compact membership summaries without weakening those guarantees.

### Atomic Intent Bundles

Some domain operations span several rows and must not be observed partially.
Loomabase should model signed, idempotent intent bundles that preserve domain
atomicity during retries and synchronization, rather than pretending every
business invariant can be reduced to independent cell registers.

## Deliberate Non-Goals

- Claiming strong consistency while devices are offline.
- Hiding merge policy behind undocumented magic.
- Treating authentication, RLS, backups, or monitoring as optional.
- Adding CRDT types without executable laws and operational tooling.

## Delivery Order

1. Generate typed SDKs and richer policy hooks from `TableDef`.
2. Stabilize the pre-1.0 wire protocol and compatibility policy.
3. Add safe tombstone compaction and compact partial-replica membership summaries.
4. Expand the deterministic simulation into a conformance toolkit.
5. Add compact anti-entropy and conflict explainability.
6. Add policy-aware offline writes and atomic intent bundles.
