import {
  LocalStorageTodoReplicaStorage,
  LoomabaseHttpClient,
  MemoryTodoReplica,
} from "../../packages/loomabase-js/dist/index.js";

const elements = {
  device: document.querySelector("#device"),
  todoId: document.querySelector("#todo-id"),
  title: document.querySelector("#title"),
  offline: document.querySelector("#offline"),
  resetServer: document.querySelector("#reset-server"),
  resetLocal: document.querySelector("#reset-local"),
  create: document.querySelector("#create"),
  phoneEdit: document.querySelector("#phone-edit"),
  desktopToggle: document.querySelector("#desktop-toggle"),
  sync: document.querySelector("#sync"),
  deviceId: document.querySelector("#device-id"),
  lamport: document.querySelector("#lamport"),
  cursor: document.querySelector("#cursor"),
  pending: document.querySelector("#pending"),
  localState: document.querySelector("#local-state"),
  serverLamport: document.querySelector("#server-lamport"),
  serverState: document.querySelector("#server-state"),
  explanations: document.querySelector("#explanations"),
  log: document.querySelector("#log"),
};

const client = new LoomabaseHttpClient({
  endpoint: new URL("/sync", window.location.origin),
});

let storage;
let replica;
let lastExplanations = [];

const params = new URLSearchParams(window.location.search);
const initialDevice = params.get("device") === "desktop" ? "desktop" : "phone";
elements.device.value = initialDevice;

await openReplica(initialDevice);
await refreshServerState();
render();

elements.device.addEventListener("change", async () => {
  const deviceId = elements.device.value;
  const nextUrl = new URL(window.location.href);
  nextUrl.searchParams.set("device", deviceId);
  window.history.replaceState(null, "", nextUrl);
  await openReplica(deviceId);
  log(`switched to ${deviceId}`);
  render();
});

elements.resetServer.addEventListener("click", () =>
  run("reset demo server", async () => {
    await fetch("/api/reset", { method: "POST" });
    await refreshServerState();
  }),
);

elements.resetLocal.addEventListener("click", () =>
  run("reset this device", async () => {
    await storage.clear?.();
    replica = new MemoryTodoReplica({ deviceId: elements.device.value });
    lastExplanations = [];
    await replica.save(storage);
  }),
);

elements.create.addEventListener("click", () =>
  run("create shared todo", async () => {
    replica.createTodo(todoId(), elements.title.value || "Shared Loomabase todo", false);
    await replica.save(storage);
  }),
);

elements.phoneEdit.addEventListener("click", () =>
  run("phone edits title", async () => {
    if (replica.deviceId !== "phone") {
      throw new Error("open this action on the phone device");
    }
    replica.updateTitle(todoId(), `Phone offline edit at ${new Date().toLocaleTimeString()}`);
    await replica.save(storage);
  }),
);

elements.desktopToggle.addEventListener("click", () =>
  run("desktop toggles completed", async () => {
    if (replica.deviceId !== "desktop") {
      throw new Error("open this action on the desktop device");
    }
    const current = replica.getTodo(todoId());
    replica.setCompleted(todoId(), !(current?.completed ?? false));
    await replica.save(storage);
  }),
);

elements.sync.addEventListener("click", () =>
  run("sync / pull", async () => {
    if (elements.offline.checked) {
      throw new Error("offline mode is enabled; local changes remain pending");
    }
    const outbound = replica.localDelta();
    const response = await client.sync(outbound);
    replica.acknowledge(outbound, response);
    lastExplanations = replica.applyRemote(response);
    await replica.save(storage);
    await refreshServerState();
  }),
);

async function openReplica(deviceId) {
  storage = new LocalStorageTodoReplicaStorage({
    key: `loomabase:phone-desktop:${deviceId}`,
  });
  replica = await MemoryTodoReplica.open({ deviceId, storage });
}

async function run(label, action) {
  try {
    await action();
    log(`ok: ${label}`);
  } catch (error) {
    log(`error: ${error instanceof Error ? error.message : String(error)}`);
  } finally {
    await refreshServerState().catch(() => undefined);
    render();
  }
}

async function refreshServerState() {
  const response = await fetch("/api/state", { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`server state failed: ${response.status}`);
  }
  elements.serverState.dataset.state = await response.text();
}

function render() {
  const delta = replica.localDelta();
  const pendingCells = delta.changes.reduce(
    (count, row) => count + Object.keys(row.columns).length,
    0,
  );
  const serverState = JSON.parse(elements.serverState.dataset.state ?? "{}");

  elements.deviceId.textContent = replica.deviceId;
  elements.lamport.textContent = replica.lamport.toString();
  elements.cursor.textContent = replica.cursor.toString();
  elements.pending.textContent = `${delta.changes.length} rows / ${pendingCells} cells`;
  elements.localState.textContent = pretty(replica.listTodos());
  elements.serverLamport.textContent = serverState.globalLamport ?? "0";
  elements.serverState.textContent = pretty(serverState.todos ?? []);
  elements.explanations.textContent = lastExplanations.length
    ? pretty(lastExplanations)
    : "No remote cells applied yet.";
}

function todoId() {
  return elements.todoId.value || "todo-1";
}

function pretty(value) {
  return JSON.stringify(
    value,
    (_key, fieldValue) =>
      typeof fieldValue === "bigint" ? fieldValue.toString() : fieldValue,
    2,
  );
}

function log(message) {
  const line = `[${new Date().toLocaleTimeString()}] ${message}`;
  elements.log.textContent = `${line}\n${elements.log.textContent}`.slice(0, 6000);
}
