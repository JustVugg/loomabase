<p align="center">
  <img src="loomabase.png" alt="loomabase" width="500"/>
</p>

# Loomabase

Loomabase is an open-source, offline-first synchronization engine for applications
that use SQLite at the edge and PostgreSQL as their system of record. It is
implemented in safe Rust at its core and resolves conflicts at column
granularity with deterministic LWW CRDT registers and Lamport clocks. The
separate optional C ABI crate necessarily contains audited `unsafe` boundaries.

> Status: production-oriented pre-1.0 engine. The merge, isolation, recovery,
> authentication, and bounded anti-entropy paths are implemented and tested.
> The wire protocol remains pre-1.0 and production rollout still requires the
> environment-specific gates in the
> [production runbook](docs/production-runbook.md).

## Why Loomabase

Most synchronization systems resolve conflicts at row or document level. That
causes unrelated offline edits to overwrite each other. Loomabase assigns CRDT
metadata to each synchronized column, so a title edited on one device and a
completion flag edited on another both survive.

The engine is deliberately transport-agnostic and framework-agnostic. It can
sit behind an HTTP, WebSocket, QUIC, or custom embedded transport without
mixing network concerns into convergence logic.

## Core Guarantees

- **Column-level convergence:** independent column updates merge without data
  loss.
- **Row lifecycle as a CRDT:** creation, deletion, and restoration are a
  per-row last-writer-wins liveness register, so deletes converge and concurrent
  column edits are preserved across a delete/restore.
- **Deterministic LWW:** `(lamport_clock, device_id)` creates a total order,
  including concurrent equal-clock writes.
- **Transactional durability:** application rows, metadata, and clocks change
  in explicit SQLite or PostgreSQL transactions.
- **Idempotent cell state:** duplicate payload delivery does not change the
  resulting cells.
- **Race-safe acknowledgement:** a newer local write cannot be cleared by an
  acknowledgement for an older version.
- **Authoritative partial replicas:** durable parameterized scopes receive
  complete membership snapshots and explicit local-only evictions. Overlapping
  scopes, concurrent in-flight requests, and dirty offline writes are handled
  without creating global tombstones or losing data.
- **Typed payloads:** SQL values retain explicit types through Serde and JSON.
- **Rolling protocol upgrades:** the current and previous wire versions are
  accepted; incompatible payloads are rejected before mutation.
- **Schema-checked sync:** every payload carries a deterministic contract
  fingerprint, so a client and server on mismatched schemas are rejected before
  any mutation rather than corrupting data.
- **Bounded untrusted input and output:** identifiers, values, row/cell counts,
  clocks, cursors, response pages, columns, and device attribution are
  validated or capped before mutation.
- **Policy-aware sync:** authenticated but unauthorized cells can be rejected
  before LWW merge through pluggable `SyncAuthorizer` and `SyncValidator`
  hooks.
- **Structured rejection protocol:** valid cells denied by authorization or
  business validation are returned in `SyncPayload.rejections`; malformed cells
  remain hard protocol errors.
- **Transactional audit log:** PostgreSQL merges can record accepted,
  idempotent, superseded, authorization-denied, and validation-denied cells in
  `loomabase_audit_log` inside the same transaction.
- **Safe Rust core:** `unsafe` is forbidden in the main crate.
- **Async integration:** PostgreSQL uses SQLx async transactions; blocking
  rusqlite operations run on Tokio's blocking pool behind a `Send + Sync`
  facade.

## How It Works

Each synchronized cell is a deterministic LWW register:

```text
(todo_id, column_name) -> (typed_value, lamport_clock, device_id)
```

An incoming value wins when:

```text
incoming.clock > current.clock
OR
incoming.clock == current.clock AND incoming.device_id > current.device_id
```

SQLite triggers atomically capture local changes in `todos_crdt`. The
PostgreSQL adapter locks the server Lamport clock and affected CRDT rows inside
the caller's transaction. Responses are bounded pages of cells written after a
server-issued, tenant/device/table-bound cursor.

See [Architecture](docs/architecture.md) for transaction boundaries, protocol
invariants, and trust assumptions. See [Vision](docs/vision.md) for the
capabilities intended to make Loomabase technically distinct.

## Repository Layout

```text
src/
  auth.rs         HS256/RS256 and Supabase JWKS authentication (`server`)
  client.rs       SQLite async facade, delta extraction, and apply logic
  crdt.rs         Typed protocol, validation, LWW ordering, reference merge
  error.rs        Unified typed errors
  http.rs         Optional HTTP surface and pluggable device authentication
  policy.rs       Authorization hooks, business validators, rejections, audit
  replica.rs      Partial-replica interests, membership, and eviction protocol
  schema.rs       Declarative table contract; generates DDL, triggers, migrations
  server.rs       Transactional, tenant-scoped PostgreSQL adapter
  server_main.rs  `loomabase-server` binary (feature `server`)
tests/
  anti_entropy.rs          Incremental cursor change feed (O(delta) server pull)
  auth.rs                  JWT verification: expiry, tampering, algorithm
  crdt_laws.rs             CRDT delivery-order and atomicity laws
  custom_contract.rs       End-to-end sync of an arbitrary (non-todos) contract
  delete_lifecycle.rs      Tombstone propagation, restore, and liveness LWW
  http_server.rs           HTTP /sync endpoint, auth, limits, and error mapping
  multi_table.rs           Multi-table contract synced per table on one database
  multi_tenancy.rs         Tenant isolation and independent per-tenant clocks
  offline_convergence.rs   Realistic multi-device offline convergence
  postgres_integration.rs  SQLx adapter against a real PostgreSQL instance
  rls.rs                   Row-Level Security isolation via a limited role
  schema_migration.rs      Additive contract migration on an existing database
  security_boundaries.rs   Untrusted-input and spoofing boundaries
  sqlite_atomicity.rs      Trigger, rollback, acknowledgement, async tests
  model_convergence.rs     Randomized reordered/duplicate delivery model
loomabase-ffi/             C ABI bindings (cdylib) for language SDKs
```

## Quick Start

Add the crate from the repository while Loomabase is pre-release:

```toml
[dependencies]
loomabase = { git = "https://github.com/loomabase/loomabase" }
```

Initialize a SQLite client:

```rust,no_run
use loomabase::Result;
use loomabase::client::SqliteClient;

#[tokio::main]
async fn main() -> Result<()> {
    let client = SqliteClient::open("edge.db", "device-01").await?;
    client
        .create_todo("todo-1".into(), "Ship Loomabase".into(), false)
        .await?;

    let outbound = client.local_delta().await?;
    // Send `outbound` through the authenticated transport.
    Ok(())
}
```

The recommended integration uses `sync_until_caught_up`, which keeps transport
concerns outside the engine, atomically acknowledges and applies every
successful page, and follows bounded server pages until caught up:

```rust,no_run
# use loomabase::{Result, client::SqliteClient, crdt::SyncPayload};
# async fn send_to_api(_: SyncPayload) -> Result<SyncPayload> { unreachable!() }
# async fn example(client: &SqliteClient) -> Result<()> {
client.sync_until_caught_up(send_to_api).await?;
# Ok(())
# }
```

Synchronize a durable partial-replica scope. The server recomputes membership
after merging local writes and returns a complete authoritative snapshot.
Rows leaving the scope are evicted locally only after dirty writes are
acknowledged; eviction never becomes a replicated delete:

```rust,no_run
# use loomabase::{Result, client::SqliteClient, replica::{PartialReplicaRequest, PartialReplicaResponse, ReplicaInterest, ReplicaPredicate}};
# async fn send_partial(_: PartialReplicaRequest) -> Result<PartialReplicaResponse> { unreachable!() }
# async fn example(client: &SqliteClient) -> Result<()> {
let interest = ReplicaInterest {
    predicates: vec![ReplicaPredicate::IdPrefix("project-a/".into())],
    limit: 10_000,
};
client
    .sync_partial_with("project-a".into(), interest, send_partial)
    .await?;
client.remove_partial_replica_scope("project-a".into()).await?;
# Ok(())
# }
```

The reference HTTP server exposes normal sync at `POST /sync` and partial
replica sync at `POST /sync/partial`.

Run the complete two-device library example:

```bash
cargo run --example offline_roundtrip
```

Run the persistent SQLite-to-HTTP-to-PostgreSQL example against a running
authenticated server:

```bash
LOOMABASE_JWT_SECRET='replace-with-at-least-32-bytes' \
  cargo run --features server --example http_offline_roundtrip

LOOMABASE_JWT_SECRET='replace-with-at-least-32-bytes' \
  cargo run --features server --example http_partial_replica
```

Merge an authenticated payload on PostgreSQL:

```rust,no_run
use loomabase::Result;
use loomabase::crdt::SyncPayload;
use loomabase::server::merge_crdt_states;
use sqlx::PgPool;

async fn synchronize(
    pool: &PgPool,
    payload: SyncPayload,
    authenticated_device_id: &str,
) -> Result<SyncPayload> {
    let mut tx = pool.begin().await?;
    let response = merge_crdt_states(
        &mut tx,
        payload,
        authenticated_device_id,
        "authenticated-tenant",
        &loomabase::schema::todos_table(),
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}
```

The API layer must authenticate the device and pass the authenticated tenant to
the merge. The merge establishes the transaction-scoped PostgreSQL RLS context.

## Arbitrary Contracts

`todos` is just the canonical contract. A [`TableDef`](src/schema.rs) declares
any single table; the SQLite and PostgreSQL schema, change-capture triggers,
validation, value codec, and materialization are all generated from it. The
generic row API (`insert`, `set`, `get_cell`, `delete`, `restore`) then
synchronizes that contract end to end.

```rust,no_run
use std::collections::BTreeMap;
use loomabase::Result;
use loomabase::client::SqliteClient;
use loomabase::crdt::CrdtValue;
use loomabase::schema::{ColumnDef, ColumnType, TableDef};

#[tokio::main]
async fn main() -> Result<()> {
    let notes = TableDef::new(
        "notes",
        vec![
            ColumnDef::new("body", ColumnType::Text),
            ColumnDef::new("priority", ColumnType::Integer),
        ],
    )?;
    let client = SqliteClient::open_with("edge.db", "device-01", notes).await?;
    client
        .insert(
            "note-1".into(),
            BTreeMap::from([
                ("body".into(), CrdtValue::Text("buy milk".into())),
                ("priority".into(), CrdtValue::Integer(2)),
            ]),
        )
        .await?;
    Ok(())
}
```

Supported column domains are `Text`, `Integer`, `Real`, and `Boolean`. Row
lifecycle (`deleted`) is a reserved liveness register managed through `delete`
and `restore`.

Contracts evolve safely: opening a client or initializing the server with a
contract that adds columns runs an automatic additive migration (`ALTER TABLE
… ADD COLUMN` plus trigger regeneration on SQLite) on the existing database.
Destructive changes — a removed or retyped column — are rejected rather than
applied. Because each replica's `schema_fingerprint` changes with the contract,
client and server only resume syncing once both sides have migrated.

## Running the Server

A reference HTTP server lives behind the optional `server` feature, keeping the
core library dependency-light:

```bash
DATABASE_URL=postgres://postgres:postgres@localhost:5432/loomabase \
  cargo run --features server --bin loomabase-server
```

It is configured by environment variables: `LOOMABASE_BIND` (default
`127.0.0.1:8080`), `LOOMABASE_JWT_PUBLIC_KEY`, `LOOMABASE_JWT_SECRET`,
`LOOMABASE_JWT_AUDIENCE`, `LOOMABASE_JWT_ISSUER`,
`LOOMABASE_SUPABASE_URL`, `LOOMABASE_SUPABASE_JWKS`,
`LOOMABASE_JWKS_REFRESH_SECS`, `LOOMABASE_SUPABASE_TENANT_CLAIM`,
`LOOMABASE_SUPABASE_TABLES_CLAIM`,
`LOOMABASE_BODY_LIMIT_BYTES`, `LOOMABASE_REQUEST_TIMEOUT_SECS`,
`LOOMABASE_MAX_CONCURRENT_REQUESTS`, `LOOMABASE_STATEMENT_TIMEOUT_SECS`,
`LOOMABASE_LOCK_TIMEOUT_SECS`, `LOOMABASE_DB_MAX_CONNECTIONS`,
`LOOMABASE_DB_MIN_CONNECTIONS`, `LOOMABASE_DB_ACQUIRE_TIMEOUT_SECS`,
`LOOMABASE_DB_TRANSACTION_POOLER`, and `RUST_LOG` (structured `tracing`
output, default `info`). It exposes
`GET /health`, `GET /metrics` (Prometheus), and `POST /sync`. It logs each
request and merge outcome, shuts down gracefully on SIGINT/SIGTERM, and warns
at startup if connected as a PostgreSQL superuser.

The sync handler authenticates the request, runs the transactional,
tenant-scoped `merge_crdt_states`, and returns the server response as JSON. It
selects Supabase asymmetric JWKS when `LOOMABASE_SUPABASE_URL` is set, otherwise
an `RS256` public-key verifier or `HS256` shared-secret verifier. Without a
configured verifier, startup fails closed. The insecure
`HeaderDeviceAuthenticator` stub can only be enabled explicitly with
`LOOMABASE_ALLOW_INSECURE_HEADERS=true` for development. Either way the
authenticated `tenant_id` — never the payload — is the isolation boundary, and
every server row and clock is keyed by it. Provide your own
`DeviceAuthenticator` implementation for other token schemes.

Production API surfaces can use `merge_crdt_states_with_security` or
`http::app_with_config_and_security` to add field authorization, business
validation, structured rejections, and transactional audit logging without
forking the merge engine. The default server remains allow-all after JWT/table
authorization, so application-specific policy should be configured explicitly.

Supabase PostgreSQL and Supabase Auth are supported directly, including
rotatable asymmetric JWKS and Supavisor transaction mode. See the
[Supabase integration guide](docs/supabase.md).

Apply schema changes with a migration role, then run the service with a
non-superuser DML-only role:

```bash
DATABASE_URL=postgres://migration-role:...@db/loomabase \
LOOMABASE_MIGRATE_ONLY=true \
  cargo run --features server --bin loomabase-server

DATABASE_URL=postgres://runtime-role:...@db/loomabase \
LOOMABASE_SKIP_SCHEMA_INIT=true \
LOOMABASE_JWT_PUBLIC_KEY='...' \
  cargo run --features server --bin loomabase-server
```

The runtime role needs DML rights on Loomabase tables plus `USAGE, SELECT` on
`loomabase_seq`; it must also have DML rights on `loomabase_audit_log` when
database audit is enabled. It must not be a PostgreSQL superuser.

## Language SDKs

The `loomabase-ffi` crate exposes the CRDT protocol core over a small, stable C
ABI (see `loomabase-ffi/include/loomabase.h`) as the foundation for Swift,
Kotlin, C, or Python (cffi) SDKs. Handles serialize concurrent merges, expose an
ABI version, and provide thread-local error diagnostics. A caller JSON-encodes
a `SyncPayload`, merges it for a device, and receives the JSON server response;
storage and transport stay on the host side. It is a separate workspace crate
because the C ABI requires `unsafe`, which the core crate forbids.

```bash
cargo build -p loomabase-ffi --release   # builds the cdylib
```

The contract code generator emits typed Swift, Kotlin, TypeScript, and Dart
models plus a transport interface:

```bash
cargo run --example generate_sdks
```

Conflict decisions can be explained through `explain::explain_lww`. The
`replica` module implements revisioned authoritative membership, safe
local-only eviction, and validated query planning. The deterministic network
simulator can produce a visual report:

```bash
cargo run --example simulation_report
cargo run --release --bin loomabase-bench
```

## Development and Verification

Run the local quality gate:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Run the PostgreSQL integration test:

```bash
docker run --rm --name loomabase-postgres \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=loomabase \
  -p 5432:5432 postgres:17-alpine

LOOMABASE_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/loomabase \
  cargo test --test postgres_integration
```

CI runs all tests against PostgreSQL, Clippy with warnings denied, formatting,
`cargo audit`, and `cargo deny`; scheduled jobs run the ignored soak target and
fuzz both base-sync and partial-replica protocol inputs. Use
[`ops/k6-sync.js`](ops/k6-sync.js) and the
[public benchmark methodology](benchmarks/README.md) for reproducible load
tests.

## Security & Best Practices

Read [SECURITY.md](SECURITY.md) before production deployment. Loomabase treats
every sync payload as untrusted input. The CRDT merge is deterministic and
idempotent, but it is not an authorization system: a valid authenticated writer
can still submit a valid newer value. Use Loomabase together with
application-specific authorization, schema contracts, business validation, rate
limits, and audit logging.

### Malicious or Malformed CRDT Updates

Loomabase rejects malformed or unsafe payloads before mutation:

- protocol versions must be supported during the rolling-upgrade window;
- schema fingerprints must match the server contract;
- table and column names come from validated `TableDef` contracts, never from
  payload-controlled SQL identifiers;
- values are type-checked against the contract and written with SQL parameters;
- row counts, cell counts, identifier lengths, value sizes, response sizes,
  finite numbers, cursor capabilities, and Lamport clock advances are bounded;
- the authenticated `device_id` must match every submitted mutation, preventing
  device-attribution spoofing;
- the authenticated `tenant_id`, not any payload field, scopes the PostgreSQL
  transaction and Row-Level Security context.

Once a payload is valid and authorized, conflicts are resolved per column with
the documented LWW order: `(lamport_clock, device_id)`. Older writes and
duplicate deliveries are harmless. Equal versions with different values are
rejected because one CRDT version cannot safely identify two different states.
For user-facing diagnostics, expose `explain::explain_lww` so operators can see
which value won and why.

Valid but unauthorized cells are not silently ignored. They can be returned to
the client in `SyncPayload.rejections`, kept dirty on SQLite by
`complete_sync`, and written to `loomabase_audit_log` when database audit is
enabled.

### Data Validation Before Sync

Validate data at the application boundary before it reaches the sync engine.
Recommended checks:

- enforce per-user and per-tenant write permissions before calling
  `merge_crdt_states`;
- reject fields the authenticated user cannot edit, even if the table contract
  contains them;
- validate domain rules such as length, enum membership, numeric ranges,
  foreign-key ownership, and state transitions;
- keep secrets, access tokens, API keys, and password material out of
  synchronized tables;
- use PostgreSQL constraints and forced RLS as defense in depth, not as a
  replacement for API-layer authorization;
- store audit events for security-relevant writes and conflict decisions.

For Supabase deployments, keep tenant and table authorization in trusted
`app_metadata` claims. Do not rely on user-editable `user_metadata` for sync
authorization.

Example policy wiring:

```rust,no_run
use std::sync::Arc;
use loomabase::policy::{
    AuditMode, ColumnAllowListAuthorizer, MaxTextLengthValidator, SyncSecurity,
};
use loomabase::schema::todos_table;

let table = todos_table();
let security = SyncSecurity::new(
    Arc::new(ColumnAllowListAuthorizer::new(&table, ["title", "completed"])?),
    Arc::new(MaxTextLengthValidator::columns(&table, 200, ["title"])?),
    AuditMode::Database,
);
```

Pass that `SyncSecurity` to `merge_crdt_states_with_security` or
`app_with_config_and_security`. A denied cell is returned as a structured
rejection instead of being merged; malformed payloads still fail the whole
request before mutation.

### Sync Endpoint Hardening

Expose `/sync` and `/sync/partial` only over TLS and require a signed
`Authorization: Bearer` token. In production, put the reference server behind a
gateway or load balancer that applies body-size, request-rate, connection, and
IP reputation limits in addition to Loomabase's built-in request, body,
concurrency, statement, lock, and pool limits.

For browser clients, configure CORS as an allowlist:

```http
Access-Control-Allow-Origin: https://app.example.com
Access-Control-Allow-Methods: POST, OPTIONS
Access-Control-Allow-Headers: Authorization, Content-Type
Access-Control-Max-Age: 600
Vary: Origin
```

Avoid `Access-Control-Allow-Origin: *` on authenticated sync endpoints, and do
not combine wildcard origins with credentials. If cookies are used instead of
bearer tokens, require `SameSite=Lax` or `SameSite=Strict`, `Secure`, and CSRF
protection.

Content Security Policy belongs on the browser application and any HTML served
by your gateway. A sync endpoint that returns JSON does not need to execute
script, but the web app should restrict where it can send sync traffic:

```http
Content-Security-Policy: default-src 'self'; connect-src 'self' https://api.example.com https://*.supabase.co; object-src 'none'; base-uri 'none'; frame-ancestors 'none'
```

Also set `Content-Type: application/json`, `X-Content-Type-Options: nosniff`,
`Cache-Control: no-store` for sync responses, `Referrer-Policy: no-referrer`,
and HSTS at the TLS termination layer.

## Roadmap

- Stateless signed cursors or compact summaries for deployments that do not
  want server-side opaque cursor capabilities
- Schema registry and generated typed table adapters
- Broader protocol fuzzing and fault injection
- Kubernetes/Terraform deployment modules
- Stable, versioned wire protocol

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). Changes to merge behavior must include
tests proving convergence under duplicate and reordered delivery.

## License

Loomabase is licensed under the [Apache License 2.0](LICENSE)
