import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  JsonFileTodoReplicaStorage,
  MemoryTodoReplica,
  ReferenceSyncServer,
} from "../packages/loomabase-js/dist/index.js";

const directory = await mkdtemp(join(tmpdir(), "loomabase-js-persistent-"));
try {
  const storage = new JsonFileTodoReplicaStorage(join(directory, "device-a.json"));
  const server = new ReferenceSyncServer();

  let replica = await MemoryTodoReplica.open({
    deviceId: "device-a",
    storage,
  });

  replica.createTodo("todo-1", "Persisted local todo", false);
  let outbound = replica.localDelta();
  let response = server.merge(outbound, replica.deviceId);
  replica.completeSync(outbound, response);
  await replica.save(storage);

  replica = await MemoryTodoReplica.open({
    deviceId: "device-a",
    storage,
  });

  console.log(JSON.stringify(replica.listTodos(), null, 2));
} finally {
  await rm(directory, { recursive: true, force: true });
}
