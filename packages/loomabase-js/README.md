# @loomabase/client

TypeScript/JavaScript client primitives for Loomabase.

Status: initial Node/Electron/browser-friendly SDK. It provides wire-compatible
types, safe JSON handling for Loomabase `u64` fields, an HTTP transport client,
a reference in-memory sync server for tests/demos, an in-memory todo replica,
Node/Electron JSON-file snapshots, and browser `localStorage` snapshots. Native
SQLite bindings for Node/Electron and browser WASM SQLite are next steps.

```bash
npm install @loomabase/client
```

Repository pre-release usage:

```bash
npm install ./packages/loomabase-js
```

```ts
import {
  JsonFileTodoReplicaStorage,
  LocalStorageTodoReplicaStorage,
  LoomabaseHttpClient,
  MemoryTodoReplica,
  TODOS_TABLE,
} from "@loomabase/client";

const storage = new JsonFileTodoReplicaStorage("./edge-replica.json");
const replica = await MemoryTodoReplica.open({
  deviceId: "device-web-01",
  table: TODOS_TABLE,
  storage,
});

replica.createTodo("todo-1", "Ship offline mode", false);

const client = new LoomabaseHttpClient({
  endpoint: "https://api.example.com/sync",
  getToken: async () => supabaseAccessToken,
});

await replica.syncWith((payload) => client.sync(payload));
await replica.save(storage);
```

Loomabase's Rust protocol uses `u64` for Lamport clocks and schema
fingerprints. JavaScript cannot represent every `u64` exactly as a `number`, so
this SDK represents protocol counters as `bigint` and ships
`stringifySyncPayload` / `parseSyncPayload` for exact JSON round trips.

For browser-only demos or prototypes, use `LocalStorageTodoReplicaStorage`:

```ts
const storage = new LocalStorageTodoReplicaStorage({
  key: "loomabase:todos",
});
const replica = await MemoryTodoReplica.open({
  deviceId: "browser-device",
  storage,
});
```

Run the SDK smoke tests:

```bash
npm --prefix packages/loomabase-js run check
node examples/phone_desktop_offline_reconnect.mjs
node demo/phone-desktop/server.mjs
```

Then open the desktop and phone URLs printed by the demo server. The manual
scenario proves offline local writes, reconnect, idempotent replay, and
cross-device convergence.
