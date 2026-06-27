# 5-Minute Supabase Quickstart

This guide connects an existing Supabase project to Loomabase and a
TypeScript/JavaScript client. It is intentionally narrow: one `todos` contract,
Supabase Auth, Supabase PostgreSQL, and the initial Node/Electron-oriented npm
SDK.

## 1. Install the JavaScript SDK

Published package:

```bash
npm install @loomabase/client
```

Repository pre-release:

```bash
npm install ./packages/loomabase-js
```

The initial SDK provides protocol-safe JSON, an HTTP client, an in-memory todo
replica, Node/Electron JSON-file snapshots, and browser `localStorage`
snapshots. Native SQLite storage for Node/Electron and browser WASM SQLite are
next steps.

## 2. Migrate Supabase PostgreSQL

Run migrations with a privileged migration connection, not the long-lived app
runtime role:

```bash
DATABASE_URL='postgresql://postgres:...@db.<project-ref>.supabase.co:5432/postgres' \
LOOMABASE_MIGRATE_ONLY=true \
  cargo run --release --features server --bin loomabase-server
```

Create the limited runtime role:

```bash
psql "$DATABASE_URL" -f supabase/runtime-role.sql
```

Then create a login and grant the group role:

```sql
CREATE ROLE loomabase_app LOGIN PASSWORD 'use-a-generated-secret';
GRANT loomabase_runtime TO loomabase_app;
```

Do not run normal sync traffic as the Supabase `postgres` role. Superusers can
bypass RLS.

## 3. Start Loomabase With Supabase Auth

Use the runtime role and Supabase JWKS:

```bash
DATABASE_URL='postgresql://loomabase_app:...@db.<project-ref>.supabase.co:5432/postgres' \
LOOMABASE_SKIP_SCHEMA_INIT=true \
LOOMABASE_SUPABASE_URL='https://<project-ref>.supabase.co' \
LOOMABASE_SUPABASE_TENANT_CLAIM='tenant_id' \
LOOMABASE_BIND='127.0.0.1:8080' \
  cargo run --release --features server --bin loomabase-server
```

For production, place this behind TLS and a gateway that enforces CORS, request
rate limits, and body-size limits. See [SECURITY.md](../SECURITY.md).

## 4. Sync From JavaScript

```ts
import { createClient } from "@supabase/supabase-js";
import {
  LocalStorageTodoReplicaStorage,
  LoomabaseHttpClient,
  MemoryTodoReplica,
  TODOS_TABLE,
} from "@loomabase/client";

const supabase = createClient(
  "https://<project-ref>.supabase.co",
  "<public-anon-key>",
);

const deviceKey = "loomabase_device_id";
let deviceId = window.localStorage.getItem(deviceKey);
if (!deviceId) {
  deviceId = crypto.randomUUID();
  window.localStorage.setItem(deviceKey, deviceId);
}

const storage = new LocalStorageTodoReplicaStorage({
  key: "loomabase:todos",
});
const replica = await MemoryTodoReplica.open({
  deviceId,
  table: TODOS_TABLE,
  storage,
});

const loomabase = new LoomabaseHttpClient({
  endpoint: "https://api.example.com/sync",
  getToken: async () => {
    const { data, error } = await supabase.auth.getSession();
    if (error) throw error;
    return data.session?.access_token;
  },
});

replica.createTodo("todo-1", "Try Loomabase offline", false);
await replica.syncWith((payload) => loomabase.sync(payload));
await replica.save(storage);

console.log(replica.listTodos());
```

The server uses the authenticated Supabase JWT to derive tenant/device/table
authorization. The payload is not trusted for tenant isolation.

## 5. Handle Rejections

Malformed payloads fail the whole request. Valid cells denied by policy or
business validation come back as structured rejections:

```ts
const outbound = replica.localDelta();
const response = await loomabase.sync(outbound);

if (response.rejections?.length) {
  for (const rejection of response.rejections) {
    console.warn(
      `${rejection.todo_id}.${rejection.column_name} rejected: ${rejection.reason}`,
    );
  }
}

replica.completeSync(outbound, response);
await replica.save(storage);
```

Rejected exact cell versions remain dirty locally, so your UI can show the user
what needs to be changed instead of silently dropping their offline write.

## Try The Visual Demo

```bash
npm --prefix packages/loomabase-js run build
python3 -m http.server 5173
```

Open:

```text
http://localhost:5173/demo/offline-conflicts/
```

The demo is in-memory and dependency-free. It is designed to explain Loomabase
conflict semantics before wiring a real app to PostgreSQL.
