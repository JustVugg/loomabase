import {
  MemoryTodoReplica,
  ReferenceSyncServer,
  explainLww,
} from "../../packages/loomabase-js/dist/index.js";

let server;
let deviceA;
let deviceB;
let events;

const todoId = "demo/todo-1";

const elements = {
  a: document.querySelector("#device-a"),
  b: document.querySelector("#device-b"),
  server: document.querySelector("#server"),
  explanation: document.querySelector("#explanation"),
  log: document.querySelector("#log"),
};

document.querySelector("#reset").addEventListener("click", reset);
document.querySelector("#independent").addEventListener("click", runIndependentEdits);
document.querySelector("#same-column").addEventListener("click", runSameColumnConflict);
document.querySelector("#sync-a").addEventListener("click", () => syncDevice(deviceA, "Device A"));
document.querySelector("#sync-b").addEventListener("click", () => syncDevice(deviceB, "Device B"));
document.querySelector("#pull-both").addEventListener("click", pullBoth);

reset();

function reset() {
  server = new ReferenceSyncServer();
  deviceA = new MemoryTodoReplica({ deviceId: "device-a" });
  deviceB = new MemoryTodoReplica({ deviceId: "device-b" });
  events = [];

  deviceA.createTodo(todoId, "Initial task", false);
  syncDevice(deviceA, "Device A");
  syncDevice(deviceB, "Device B");
  explain("Ready. Both devices have the same todo and can now edit offline.");
  render();
}

function runIndependentEdits() {
  deviceA.updateTitle(todoId, "Write the launch post");
  deviceB.setCompleted(todoId, true);
  log("Offline: Device A changed title, Device B changed completed.");
  explain(
    "Different columns do not conflict. The title cell and completed cell carry independent CRDT metadata.",
  );
  render();
}

function runSameColumnConflict() {
  deviceA.updateTitle(todoId, "Title from A");
  deviceB.updateTitle(todoId, "Title from B");
  const current = deviceA.getCell(todoId, "title");
  const incoming = deviceB.getCell(todoId, "title");
  explain(JSON.stringify(explainLww(current, incoming), bigintJson, 2));
  log("Offline: both devices changed title at the same logical clock.");
  render();
}

function syncDevice(replica, label) {
  const outbound = replica.localDelta();
  const response = server.merge(outbound, replica.deviceId);
  const explanations = replica.completeSync(outbound, response);
  const changedCells = outbound.changes.reduce(
    (count, row) => count + Object.keys(row.columns).length,
    0,
  );
  log(`${label} synced ${changedCells} local cell(s), received ${response.changes.length} row(s).`);
  const meaningful = explanations.find((item) => item.winner !== "equal");
  if (meaningful) {
    explain(JSON.stringify(meaningful, bigintJson, 2));
  }
  render();
}

function pullBoth() {
  syncDevice(deviceA, "Device A");
  syncDevice(deviceB, "Device B");
}

function render() {
  renderReplica(elements.a, deviceA);
  renderReplica(elements.b, deviceB);
  renderServer(elements.server, server);
  elements.log.innerHTML = events.map((event) => `<li>${escapeHtml(event)}</li>`).join("");
}

function renderReplica(target, replica) {
  const todos = replica.listTodos();
  target.innerHTML =
    todos.map(renderTodo).join("") +
    `<div class="meta">lamport=${replica.lamport} cursor=${replica.cursor}</div>`;
}

function renderServer(target, referenceServer) {
  const byRow = new Map();
  for (const [key, column] of referenceServer.cells.entries()) {
    const [rowId, columnName] = splitCellKey(key);
    const row = byRow.get(rowId) ?? { id: rowId, title: "", completed: false, deleted: false };
    if (columnName === "title" && column.value.type === "text") row.title = column.value.value;
    if (columnName === "completed" && column.value.type === "boolean") {
      row.completed = column.value.value;
    }
    if (columnName === "deleted" && column.value.type === "boolean") {
      row.deleted = column.value.value;
    }
    byRow.set(rowId, row);
  }
  const rows = Array.from(byRow.values()).filter((todo) => !todo.deleted);
  target.innerHTML =
    rows.map(renderTodo).join("") +
    `<div class="meta">lamport=${referenceServer.globalLamport}</div>`;
}

function renderTodo(todo) {
  return `<div class="todo">
    <strong>${escapeHtml(todo.title || "(empty title)")}</strong>
    <div>${todo.completed ? "completed" : "not completed"}</div>
    <div class="meta">id=${escapeHtml(todo.id)}</div>
  </div>`;
}

function log(message) {
  events.unshift(message);
  events = events.slice(0, 12);
}

function explain(message) {
  elements.explanation.textContent = message;
}

function bigintJson(_key, value) {
  return typeof value === "bigint" ? value.toString() : value;
}

function splitCellKey(key) {
  const separator = key.indexOf("|");
  return [
    decodeURIComponent(key.slice(0, separator)),
    decodeURIComponent(key.slice(separator + 1)),
  ];
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}
