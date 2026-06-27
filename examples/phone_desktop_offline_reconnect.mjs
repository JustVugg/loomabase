import assert from "node:assert/strict";

import {
  MemoryTodoReplica,
  ReferenceSyncServer,
} from "../packages/loomabase-js/dist/index.js";

const server = new ReferenceSyncServer();
const phone = new MemoryTodoReplica({ deviceId: "phone" });
const desktop = new MemoryTodoReplica({ deviceId: "desktop" });

function sync(replica) {
  const outbound = replica.localDelta();
  const response = server.merge(outbound, replica.deviceId);
  replica.completeSync(outbound, response);
  return { outbound, response };
}

function printStep(message, value) {
  console.log(`\n${message}`);
  if (value !== undefined) {
    console.log(JSON.stringify(value, null, 2));
  }
}

phone.createTodo("todo-1", "Initial shared todo", false);
sync(phone);
sync(desktop);

printStep("1. Phone and desktop start from the same synchronized state.", {
  phone: phone.listTodos(),
  desktop: desktop.listTodos(),
});

phone.updateTitle("todo-1", "Edited on the phone while offline");
desktop.setCompleted("todo-1", true);

assert.equal(phone.localDelta().changes.length, 1);
assert.equal(desktop.localDelta().changes.length, 1);

printStep("2. Both devices are offline. Local changes are still durable dirty cells.", {
  phonePendingRows: phone.localDelta().changes.length,
  desktopPendingRows: desktop.localDelta().changes.length,
  phone: phone.listTodos(),
  desktop: desktop.listTodos(),
});

sync(phone);
const desktopSync = sync(desktop);
const replay = server.merge(desktopSync.outbound, desktop.deviceId);
desktop.applyRemote(replay);
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

printStep("3. After reconnect, both devices converge without losing either edit.", {
  phone: phone.listTodos(),
  desktop: desktop.listTodos(),
});

console.log(`
PASS: offline -> reconnect -> converge works.

Why this works:
- The phone and desktop can write locally without the server because each cell
  carries its own Lamport clock and device ID.
- The server merges by column, not by whole row, so the phone title and desktop
  completed flag do not overwrite each other.
- Replaying the same desktop payload is idempotent: it does not create a second
  logical update or corrupt the final state.
- A later pull returns only the cells the other device has not seen yet.
`);
