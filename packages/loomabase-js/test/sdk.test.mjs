import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  JsonFileTodoReplicaStorage,
  LocalStorageTodoReplicaStorage,
  MemoryTodoReplica,
  ReferenceSyncServer,
  TODOS_TABLE,
  explainLww,
  fingerprintContract,
  makeColumn,
  parseSyncPayload,
  stringifySyncPayload,
  text,
} from "../dist/index.js";

test("computes the canonical todos contract fingerprint exactly", () => {
  assert.equal(fingerprintContract(TODOS_TABLE), 11482794215703764405n);
});

test("serializes and parses u64 wire fields without JavaScript precision loss", () => {
  const payload = new MemoryTodoReplica({ deviceId: "device-a" }).localDelta();
  const json = stringifySyncPayload(payload);
  assert.match(json, /"schema_fingerprint":11482794215703764405/);
  const decoded = parseSyncPayload(json);
  assert.equal(decoded.schema_fingerprint, 11482794215703764405n);
});

test("reference server converges independent offline column edits", () => {
  const server = new ReferenceSyncServer();
  const alice = new MemoryTodoReplica({ deviceId: "device-a" });
  const bob = new MemoryTodoReplica({ deviceId: "device-b" });

  alice.createTodo("todo-1", "initial", false);
  let sent = alice.localDelta();
  let response = server.merge(sent, "device-a");
  alice.completeSync(sent, response);

  sent = bob.localDelta();
  response = server.merge(sent, "device-b");
  bob.completeSync(sent, response);

  alice.updateTitle("todo-1", "from Alice");
  bob.setCompleted("todo-1", true);

  sent = alice.localDelta();
  response = server.merge(sent, "device-a");
  alice.completeSync(sent, response);

  sent = bob.localDelta();
  response = server.merge(sent, "device-b");
  bob.completeSync(sent, response);

  sent = alice.localDelta();
  response = server.merge(sent, "device-a");
  alice.completeSync(sent, response);

  assert.deepEqual(alice.getTodo("todo-1"), {
    id: "todo-1",
    title: "from Alice",
    completed: true,
    deleted: false,
  });
});

test("phone and desktop reconnect after offline edits and converge", () => {
  const server = new ReferenceSyncServer();
  const phone = new MemoryTodoReplica({ deviceId: "phone" });
  const desktop = new MemoryTodoReplica({ deviceId: "desktop" });

  const sync = (replica) => {
    const outbound = replica.localDelta();
    const response = server.merge(outbound, replica.deviceId);
    replica.completeSync(outbound, response);
    return { outbound, response };
  };

  phone.createTodo("todo-1", "Initial shared todo", false);
  sync(phone);
  sync(desktop);

  phone.updateTitle("todo-1", "Edited on the phone while offline");
  desktop.setCompleted("todo-1", true);

  assert.equal(phone.localDelta().changes.length, 1);
  assert.equal(desktop.localDelta().changes.length, 1);

  sync(phone);

  const desktopOutbound = desktop.localDelta();
  const desktopResponse = server.merge(desktopOutbound, desktop.deviceId);
  const replayResponse = server.merge(desktopOutbound, desktop.deviceId);
  desktop.completeSync(desktopOutbound, desktopResponse);

  assert.deepEqual(desktop.applyRemote(replayResponse), [
    {
      winner: "equal",
      reason: "same_version_same_value",
      current: desktop.getCell("todo-1", "title")?.metadata,
      incoming: desktop.getCell("todo-1", "title")?.metadata,
      summary: "incoming value is an idempotent replay of the current version",
    },
    {
      winner: "equal",
      reason: "same_version_same_value",
      current: desktop.getCell("todo-1", "completed")?.metadata,
      incoming: desktop.getCell("todo-1", "completed")?.metadata,
      summary: "incoming value is an idempotent replay of the current version",
    },
  ]);

  sync(phone);

  const expected = {
    id: "todo-1",
    title: "Edited on the phone while offline",
    completed: true,
    deleted: false,
  };
  assert.deepEqual(phone.getTodo("todo-1"), expected);
  assert.deepEqual(desktop.getTodo("todo-1"), expected);
  assert.deepEqual(phone.localDelta().changes, []);
  assert.deepEqual(desktop.localDelta().changes, []);
});

test("conflict explanation identifies device-id tie breaks", () => {
  const current = makeColumn(text("A"), 7n, "device-a");
  const incoming = makeColumn(text("B"), 7n, "device-b");
  const explanation = explainLww(current, incoming);
  assert.equal(explanation.winner, "incoming");
  assert.equal(explanation.reason, "device_id_tie_break");
});

test("rejected versions stay dirty in the memory replica", () => {
  const replica = new MemoryTodoReplica({ deviceId: "device-a" });
  replica.createTodo("todo-1", "invalid", false);
  const sent = replica.localDelta();
  const title = sent.changes[0].columns.title;
  replica.acknowledge(sent, {
    ...sent,
    source_device_id: "loomabase-server",
    source_lamport: sent.source_lamport + 1n,
    changes: [],
    rejections: [
      {
        todo_id: "todo-1",
        column_name: "title",
        kind: "validation_failed",
        reason: "title is invalid",
        value: title.value,
        metadata: title.metadata,
      },
    ],
  });

  const retry = replica.localDelta();
  assert.ok(retry.changes[0].columns.title);
  assert.equal(retry.changes[0].columns.title.value.type, "text");
});

test("JSON file storage persists and reloads a Node/Electron replica", async () => {
  const directory = await mkdtemp(join(tmpdir(), "loomabase-js-"));
  try {
    const storage = new JsonFileTodoReplicaStorage(join(directory, "replica.json"));
    const replica = new MemoryTodoReplica({ deviceId: "device-a" });
    replica.createTodo("todo-1", "stored locally", false);
    await replica.save(storage);

    const reopened = await MemoryTodoReplica.open({
      deviceId: "device-a",
      storage,
    });

    assert.deepEqual(reopened.getTodo("todo-1"), {
      id: "todo-1",
      title: "stored locally",
      completed: false,
      deleted: false,
    });
    assert.equal(reopened.localDelta().changes.length, 1);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("localStorage adapter persists browser replicas", async () => {
  const backing = new Map();
  const storageLike = {
    getItem: (key) => backing.get(key) ?? null,
    setItem: (key, value) => backing.set(key, value),
    removeItem: (key) => backing.delete(key),
  };
  const storage = new LocalStorageTodoReplicaStorage({
    key: "loomabase:test",
    storage: storageLike,
  });
  const replica = new MemoryTodoReplica({ deviceId: "device-web" });
  replica.createTodo("todo-1", "browser local", true);
  await replica.save(storage);

  const reopened = await MemoryTodoReplica.open({
    deviceId: "device-web",
    storage,
  });

  assert.deepEqual(reopened.getTodo("todo-1"), {
    id: "todo-1",
    title: "browser local",
    completed: true,
    deleted: false,
  });
});
