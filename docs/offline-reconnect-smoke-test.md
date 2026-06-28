# Offline Reconnect Smoke Test

This guide proves the user-facing Loomabase flow:

```text
phone goes offline -> phone edits title
desktop goes offline -> desktop toggles completed
both reconnect -> both devices converge
```

The test uses the JavaScript SDK and a local Node.js demo server. It does not
require PostgreSQL. The production path uses the same wire payload shape and
merge semantics through the Rust/PostgreSQL server.

## Automated Test

From the repository root:

```bash
npm --prefix packages/loomabase-js run build
node examples/phone_desktop_offline_reconnect.mjs
```

Expected output ends with:

```text
PASS: offline -> reconnect -> converge works.
```

What this proves:

- both devices can keep local dirty cells while offline;
- a phone edit to `title` and a desktop edit to `completed` are merged without
  overwriting each other;
- replaying the same desktop payload is idempotent;
- the final phone and desktop states are identical.

## Real Phone + Desktop Browser Test

Start the demo server:

```bash
npm --prefix packages/loomabase-js run build
node demo/phone-desktop/server.mjs
```

Open the desktop page:

```text
http://localhost:8787/demo/phone-desktop/?device=desktop
```

Open the phone page using the LAN URL printed by the server:

```text
http://192.168.x.x:8787/demo/phone-desktop/?device=phone
```

Then run the manual scenario:

1. Click `Reset demo server` once.
2. Click `Reset this device` on both phone and desktop.
3. On desktop, click `Create shared todo`, then `Sync / Pull`.
4. On phone, click `Sync / Pull` and confirm the todo appears.
5. Enable `Offline mode` on both pages, or physically disconnect one device
   after the page has loaded.
6. On phone, click `Phone edits title`.
7. On desktop, click `Desktop toggles completed`.
8. Disable `Offline mode` or reconnect the network.
9. Click `Sync / Pull` on phone, then desktop, then phone again.

Expected result:

```text
title     = phone's offline title
completed = desktop's offline completed flag
```

Both devices and the demo server show the same todo.

## Why The Result Is Correct

Loomabase tracks each synchronized column as an independent CRDT register:

```text
(todo_id, column_name) -> (typed_value, lamport_clock, device_id)
```

The winning value is deterministic:

```text
incoming.clock > current.clock
OR
incoming.clock == current.clock AND incoming.device_id > current.device_id
```

That means unrelated offline edits do not fight at row level. The phone updates
`title`; the desktop updates `completed`. Those are different registers, so the
server accepts both and later syncs return the cells each device has not seen.

Duplicate delivery is safe because a cell already present with the same
`lamport_clock` and `device_id` is the same logical version. Loomabase treats
that as an idempotent replay, not as a new write.

The demo server is intentionally unauthenticated so it is easy to run locally.
For production, replace it with the Rust server or your own endpoint using
Loomabase's authentication, authorization, validation, and audit hooks.
