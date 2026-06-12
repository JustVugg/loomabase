# Loomabase Architecture

## Goal

Loomabase provides an embeddable Rust core for bidirectional, offline-first
synchronization between a SQLite edge database and PostgreSQL. The unit of
conflict resolution is a column, not a row, so independent offline edits do
not overwrite one another.

## Cell Register

Each synchronized cell is represented by:

```text
(row_id, column_name) -> (typed_value, lamport_clock, device_id)
```

The winner is selected by lexicographically comparing:

```text
(lamport_clock, device_id)
```

This creates a total deterministic order. For a fixed set of valid mutations,
delivery order and duplicate delivery cannot change the final cell state.

## Row Lifecycle

Row existence is modeled as one more LWW register per row, stored in the
reserved `deleted` column. Creation writes `deleted = false`, deletion writes
`deleted = true`, and restoration writes `deleted = false` again, each with a
fresh `(lamport_clock, device_id)`. Whether a row is visible is therefore a
last-writer-wins decision over that single register, reusing the same merge,
validation, and delta machinery as any other column.

Two consequences follow from treating liveness as an independent register:

- A create or restore with a higher clock deterministically wins over an older
  delete, and a delete with a higher clock wins over an older create.
- A concurrent edit to a *different* column does not resurrect a deleted row.
  The edit is preserved and becomes visible again only after an explicit
  restore, which is the deliberate, documented semantics: a tombstone is sticky
  until liveness is explicitly rewritten.

Reads filter `deleted = 1` rows, so a tombstoned row is absent from the
application view on every device once the delete has converged.

## Client Transaction Boundary

Native SQLite triggers capture local inserts and updates in `todos_crdt`.
The trigger increments the client Lamport clock and writes the materialized
value and metadata in the same SQLite transaction as the application change.

Remote changes set `applying_remote = 1` inside the applying transaction.
This prevents remote values from being reclassified as local mutations. A
rollback restores both application data and trigger state.

The `dirty` bit is version-aware. Acknowledgement clears it only when the
stored `(lamport_clock, device_id)` still equals the sent version, preventing
a concurrent local write from being lost.

## Server Transaction Boundary

The caller opens a SQLx PostgreSQL transaction and passes it to
`merge_crdt_states`. The merge:

1. Validates the complete untrusted payload before writing.
2. Lazily creates and locks only the authenticated tenant's `lamport` clock row,
   so concurrent tenants advance their clocks without serializing against each
   other.
3. Locks each existing cell before applying LWW comparison.
4. Updates the materialized `todos` table and `todos_crdt` metadata together.
5. Computes the response from the same transaction snapshot.

The caller commits only after successfully serializing the response. Dropping
or rolling back the transaction prevents partial merges.

## Protocol Frontier

Every request and response carries an explicit protocol version. The current
and immediately previous versions are accepted for rolling upgrades; unknown
versions are rejected before any database mutation.

Every payload also carries a deterministic `schema_fingerprint` derived from the
table contract (name and ordered typed columns, including the reserved liveness
register). The receiver compares it against its own contract and rejects a
mismatch before any mutation, so a client and server that disagree about the
schema cannot silently corrupt each other's data.

`changes` contains locally dirty cells. The `PostgreSQL` adapter maintains a
monotonic per-cell `seq`, and every payload carries a cursor: a client sends the
highest server-issued `seq` page it has applied, and the server returns cells
written after it, ordered by `seq`. This makes a normal pull an
`O(changes since the cursor)` change feed instead of an `O(tenant)` scan.

Responses are capped by cell and byte budgets and carry `has_more`; clients use
`sync_until_caught_up` to apply pages atomically until caught up. Opaque cursor
capabilities and server-bound leases bind the accepted high-water mark to the
authenticated tenant, device, table, and data epoch. A missing, forged,
expired, future, or stale cursor sets `cursor_reset` and starts a bounded full
repair from zero rather than silently skipping state.

To prevent a malicious authenticated device from exhausting the global clock,
the core also bounds the accepted Lamport advance per synchronization request.

## Partial Replica Membership

A partial replica is a durable client scope identified by `scope_id` and a
validated `ReplicaInterest`. Each request contains the client's sorted known
membership and a monotonic `scope_version`. The server first merges local
writes, then recomputes the scope from the resulting PostgreSQL transaction
snapshot and returns:

- every current member ID;
- a complete CRDT cell snapshot for every current member;
- every previously known ID that must be evicted.

The server never silently truncates an authoritative snapshot. If a declared
scope or its serialized snapshot exceeds the protocol budget, the transaction
fails and the client must use a narrower scope. Query predicates are
parameterized and the tenant remains enforced by transaction-local RLS.

SQLite applies the response, membership replacement, acknowledgement, and
eviction in one transaction. Scope eviction is local storage management, not a
global delete, so it never writes the `deleted` CRDT register. A row is
physically removed only when no local scope references it and it has no dirty
cells. Dirty evictions persist as retryable markers until a later
acknowledgement. Overlapping scopes therefore preserve shared rows, and
out-of-order responses are rejected by comparing `scope_version`.
Unsubscribing a scope increments and retains an inactive revision before
eviction, so an in-flight response cannot recreate a removed subscription.

## Trust Boundaries

The transport must authenticate the device and supply the `tenant_id` to the
merge — the tenant is never read from the untrusted payload. Every server table
is keyed by `tenant_id`, and the merge sets a transaction-local RLS context.
Generated policies force tenant isolation even for the table owner; production
must still use a non-superuser runtime role, TLS, and gateway rate limits. The
core never interpolates payload-controlled SQL identifiers.

The reference server can verify native HS256/RS256 tokens or Supabase
asymmetric JWKS. Supabase workspace/table authorization is read from trusted
`app_metadata`, never from user-editable metadata.
