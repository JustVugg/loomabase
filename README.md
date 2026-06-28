# Loomabase

<p align="center">
  <img src="loomabase.png" alt="Loomabase" />
</p>

Loomabase is an open-source offline-first sync engine for applications that use
SQLite on clients and PostgreSQL on the server.

It resolves conflicts at **column level** with deterministic LWW CRDT registers
and Lamport clocks, so unrelated offline edits do not overwrite each other.

```text
phone edits title offline
desktop toggles completed offline
both reconnect
both changes survive
```

> Status: alpha. The Rust merge engine, SQLite client, PostgreSQL adapter,
> security boundaries, partial replicas, and JavaScript SDK smoke tests are
> implemented and tested. The public API and wire protocol are still pre-1.0.

## Demo

![Loomabase offline sync demo](docs/assets/loomabase-offline-demo.gif)

The demo shows two devices editing the same todo while offline. After reconnect,
Loomabase keeps both changes because `title` and `completed` are separate CRDT
cells.

## Try It In 2 Minutes

Clone the repo:

```bash
git clone https://github.com/JustVugg/loomabase.git
cd loomabase
```

Run the automated phone + desktop offline reconnect smoke test:

```bash
npm --prefix packages/loomabase-js install
npm --prefix packages/loomabase-js run build
node examples/phone_desktop_offline_reconnect.mjs
```

Expected result:

```text
PASS: offline -> reconnect -> converge works.
```

Run the interactive browser demo:

```bash
node demo/phone-desktop/server.mjs
```

Open:

```text
http://localhost:8787/demo/phone-desktop/?device=desktop
```

The server also prints a LAN URL that you can open on a phone.

## Why Loomabase Exists

Many sync systems resolve conflicts at row or document level. That means two
offline edits to different fields can still fight with each other.

Loomabase stores CRDT metadata per synchronized column:

```text
(row_id, column_name) -> (typed_value, lamport_clock, device_id)
```

An incoming cell wins when:

```text
incoming.clock > current.clock
OR
incoming.clock == current.clock AND incoming.device_id > current.device_id
```

This makes the merge deterministic, idempotent, and explainable.

## What Is Implemented

Core engine:

- Rust CRDT protocol types with Serde serialization.
- SQLite client using `rusqlite` and explicit transactions.
- SQLite triggers that capture local changes and Lamport clocks.
- PostgreSQL server adapter using `sqlx` transactions.
- Column-level LWW merge with deterministic device-ID tie break.
- Row lifecycle as a CRDT liveness register.
- Bounded anti-entropy cursors and partial replica scopes.

Security and correctness:

- Authenticated device attribution checks.
- Tenant-scoped PostgreSQL merge path.
- Row-Level Security support.
- Policy hooks for authorization and business validation.
- Structured rejected-cell protocol.
- Transactional audit log support.
- Fuzz targets, model convergence tests, and integration tests.

Developer surface:

- TypeScript/JavaScript SDK in `packages/loomabase-js`.
- Node/Electron JSON-file replica storage.
- Browser `localStorage` prototype storage.
- Visual offline conflict demo.
- Phone + desktop offline reconnect demo.
- Supabase quickstart and deployment notes.

## What Alpha Means

Loomabase is useful for evaluation, demos, prototypes, and early integration
work. It is not yet a hosted BaaS and it does not promise stable 1.0 wire
compatibility.

Use Loomabase today if you want:

- an auditable sync engine you can run yourself;
- SQLite to PostgreSQL offline-first sync;
- deterministic column-level conflict resolution;
- explicit control over auth, validation, deployment, and storage.

Do not choose Loomabase yet if you need:

- a hosted cloud BaaS today;
- a stable 1.0 wire protocol today;
- fully automatic schema sync without understanding conflict semantics.

Those are roadmap items, not hidden features.

## Install From GitHub

Rust pre-release dependency:

```toml
[dependencies]
loomabase = { git = "https://github.com/JustVugg/loomabase" }
```

JavaScript/TypeScript SDK from a local clone:

```bash
git clone https://github.com/JustVugg/loomabase.git
cd your-app
npm install ../loomabase/packages/loomabase-js
```

The JS package is not published to npm yet. The package is included in the repo
so early users can test the API before the first registry release.

## Minimal Rust Example

```rust,no_run
use loomabase::Result;
use loomabase::client::SqliteClient;

#[tokio::main]
async fn main() -> Result<()> {
    let client = SqliteClient::open("edge.db", "device-01").await?;
    client
        .create_todo("todo-1".into(), "Ship offline mode".into(), false)
        .await?;

    let outbound = client.local_delta().await?;
    // Send outbound through your authenticated transport.
    Ok(())
}
```

Run the Rust offline roundtrip example:

```bash
cargo run --example offline_roundtrip
```

## Minimal JS Example

```js
import {
  MemoryTodoReplica,
  ReferenceSyncServer,
} from "./packages/loomabase-js/dist/index.js";

const server = new ReferenceSyncServer();
const phone = new MemoryTodoReplica({ deviceId: "phone" });
const desktop = new MemoryTodoReplica({ deviceId: "desktop" });

function sync(replica) {
  const outbound = replica.localDelta();
  const response = server.merge(outbound, replica.deviceId);
  replica.completeSync(outbound, response);
}

phone.createTodo("todo-1", "Initial todo", false);
sync(phone);
sync(desktop);

phone.updateTitle("todo-1", "Edited offline on phone");
desktop.setCompleted("todo-1", true);

sync(phone);
sync(desktop);
sync(phone);

console.log(phone.listTodos());
```

## Repository Map

```text
src/                     Rust sync engine
tests/                   Rust integration, model, auth, RLS, and CRDT tests
packages/loomabase-js/   TypeScript/JavaScript SDK preview
demo/                    Browser demos
examples/                Runnable Rust and JavaScript examples
docs/                    Architecture, security, Supabase, and runbooks
loomabase-ffi/           Optional C ABI package
fuzz/                    Protocol fuzz targets
benchmarks/              Benchmark methodology
deploy/                  Self-hosting examples
supabase/                Supabase integration helpers
```

## Local Verification

Run checks locally:

```bash
cargo fmt --all --check
cargo test --workspace --all-targets --all-features
npm --prefix packages/loomabase-js install
npm --prefix packages/loomabase-js run check
```

PostgreSQL-specific tests use:

```bash
LOOMABASE_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/loomabase \
  cargo test --test postgres_integration
```

## Documentation

- [Architecture](docs/architecture.md)
- [Security model](SECURITY.md)
- [Supabase quickstart](docs/quickstart-supabase-5min.md)
- [Offline reconnect smoke test](docs/offline-reconnect-smoke-test.md)
- [Production runbook](docs/production-runbook.md)
- [Vision](docs/vision.md)

## Roadmap To Stability

The path to stable Loomabase is:

- freeze a versioned 1.0 wire protocol;
- publish golden JSON protocol vectors;
- publish the JS SDK to npm;
- add native SQLite-backed SDK storage layers;
- harden migrations and schema registry workflows;
- publish reproducible benchmarks;
- run broader fuzzing and external security review.

## License

Loomabase is licensed under the [Apache License 2.0](LICENSE).
