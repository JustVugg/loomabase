import {
  MemoryTodoReplica,
  ReferenceSyncServer,
} from "../packages/loomabase-js/dist/index.js";

const server = new ReferenceSyncServer();
const deviceA = new MemoryTodoReplica({ deviceId: "device-a" });
const deviceB = new MemoryTodoReplica({ deviceId: "device-b" });

function sync(replica) {
  const outbound = replica.localDelta();
  const response = server.merge(outbound, replica.deviceId);
  replica.completeSync(outbound, response);
  return response;
}

deviceA.createTodo("todo-1", "Initial title", false);
sync(deviceA);
sync(deviceB);

deviceA.updateTitle("todo-1", "Title edited offline on A");
deviceB.setCompleted("todo-1", true);

sync(deviceA);
sync(deviceB);
sync(deviceA);

console.log(JSON.stringify(deviceA.listTodos(), null, 2));
