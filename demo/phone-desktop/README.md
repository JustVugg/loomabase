# Phone + Desktop Offline Reconnect Demo

This demo is a local, dependency-free smoke test for the JavaScript SDK. It
runs a small Node.js sync server using Loomabase's reference merge engine and a
browser UI that can be opened from both a phone and a desktop on the same LAN.

It is intentionally a development demo. Production deployments should use the
Rust server or a hardened application endpoint with authentication,
authorization, validation, rate limits, TLS, and audit logging.

## Run

From the repository root:

```bash
npm --prefix packages/loomabase-js run build
node demo/phone-desktop/server.mjs
```

Open the desktop URL printed by the server:

```text
http://localhost:8787/demo/phone-desktop/?device=desktop
```

Open the LAN URL from your phone, replacing the IP with the address printed by
the server:

```text
http://192.168.x.x:8787/demo/phone-desktop/?device=phone
```

## Manual Test

1. Click `Reset demo server` once.
2. On both devices, click `Reset this device`.
3. On the desktop, click `Create shared todo`, then `Sync / Pull`.
4. On the phone, click `Sync / Pull` and confirm the todo appears.
5. Enable `Offline mode` on both pages, or disconnect one device after the page
   has loaded.
6. On the phone, click `Phone edits title`.
7. On the desktop, click `Desktop toggles completed`.
8. Disable `Offline mode` or reconnect the network.
9. Click `Sync / Pull` on the phone, then on the desktop, then on the phone
   again.

Expected result: both devices show the phone title and the desktop completed
flag. Neither edit is lost.

## Why It Works

The phone and desktop do not need the server while they are offline. Local
writes become dirty CRDT cells with a Lamport clock and device ID. When the
devices reconnect, the server merges each column independently with the
deterministic `(lamport_clock, device_id)` order. Because `title` and
`completed` are separate CRDT registers, they do not overwrite each other.

A later sync pulls only the cells a device has not seen yet. Replaying the same
payload is safe because a cell version that is already present is idempotent.
