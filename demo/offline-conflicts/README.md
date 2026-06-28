# Loomabase Offline Conflict Demo

Static visual demo for the TypeScript SDK.

It shows two offline devices editing the same todo, then synchronizing through a
reference Loomabase server in memory. It is intentionally dependency-free so it
can be inspected and modified quickly.

Build the SDK first:

```bash
npm --prefix packages/loomabase-js run build
```

Serve the repository root so browser ES module imports work:

```bash
python3 -m http.server 5173
```

Open:

```text
http://localhost:5173/demo/offline-conflicts/
```

The demo currently runs fully in memory. It is meant to explain conflict
semantics; production apps should use the Rust server and a persistent
SQLite-backed client.
