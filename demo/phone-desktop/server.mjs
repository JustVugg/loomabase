import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { networkInterfaces } from "node:os";
import { dirname, extname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import {
  ReferenceSyncServer,
  parseSyncPayload,
  stringifySyncPayload,
} from "../../packages/loomabase-js/dist/index.js";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const host = process.env.HOST ?? "0.0.0.0";
const port = Number.parseInt(process.env.PORT ?? "8787", 10);
const maxBodyBytes = 1024 * 1024;

let syncServer = new ReferenceSyncServer();

const server = createServer(async (request, response) => {
  try {
    const requestUrl = new URL(
      request.url ?? "/",
      `http://${request.headers.host ?? "localhost"}`,
    );

    if (request.method === "OPTIONS") {
      writeCors(response);
      response.writeHead(204);
      response.end();
      return;
    }

    if (request.method === "POST" && requestUrl.pathname === "/sync") {
      const body = await readRequestBody(request);
      const payload = parseSyncPayload(body);
      const syncResponse = syncServer.merge(payload, payload.source_device_id);
      writeJson(response, stringifySyncPayload(syncResponse));
      return;
    }

    if (request.method === "POST" && requestUrl.pathname === "/api/reset") {
      syncServer = new ReferenceSyncServer();
      writeJson(response, JSON.stringify(serverState()));
      return;
    }

    if (request.method === "GET" && requestUrl.pathname === "/api/state") {
      writeJson(response, JSON.stringify(serverState()));
      return;
    }

    if (request.method === "GET" || request.method === "HEAD") {
      await serveStatic(requestUrl.pathname, request.method, response);
      return;
    }

    response.writeHead(405, { "content-type": "text/plain; charset=utf-8" });
    response.end("method not allowed");
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    response.writeHead(500, { "content-type": "application/json; charset=utf-8" });
    response.end(JSON.stringify({ error: message }));
  }
});

server.listen(port, host, () => {
  console.log("Loomabase phone + desktop demo server");
  console.log(`Desktop: http://localhost:${port}/demo/phone-desktop/?device=desktop`);
  for (const address of lanAddresses()) {
    console.log(`Phone:   http://${address}:${port}/demo/phone-desktop/?device=phone`);
  }
});

async function serveStatic(pathname, method, response) {
  const normalizedPath = pathname === "/" ? "/demo/phone-desktop/" : pathname;
  const staticPath = normalizedPath.endsWith("/")
    ? `${normalizedPath}index.html`
    : normalizedPath;
  const filePath = resolve(repoRoot, `.${decodeURIComponent(staticPath)}`);
  if (filePath !== repoRoot && !filePath.startsWith(`${repoRoot}${sep}`)) {
    response.writeHead(403, { "content-type": "text/plain; charset=utf-8" });
    response.end("forbidden");
    return;
  }

  const body = await readFile(filePath);
  response.writeHead(200, {
    "content-type": contentType(filePath),
    "cache-control": "no-store",
  });
  if (method !== "HEAD") {
    response.end(body);
  } else {
    response.end();
  }
}

function writeCors(response) {
  response.setHeader("access-control-allow-origin", "*");
  response.setHeader("access-control-allow-methods", "GET, POST, OPTIONS");
  response.setHeader("access-control-allow-headers", "content-type");
  response.setHeader("access-control-max-age", "600");
}

function writeJson(response, body) {
  writeCors(response);
  response.writeHead(200, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
    "x-content-type-options": "nosniff",
  });
  response.end(body);
}

function readRequestBody(request) {
  return new Promise((resolveBody, rejectBody) => {
    let body = "";
    let rejected = false;
    request.setEncoding("utf8");
    request.on("data", (chunk) => {
      body += chunk;
      if (!rejected && Buffer.byteLength(body, "utf8") > maxBodyBytes) {
        rejected = true;
        rejectBody(new Error("request body is too large"));
        request.destroy();
      }
    });
    request.on("end", () => {
      if (!rejected) {
        resolveBody(body);
      }
    });
    request.on("error", rejectBody);
  });
}

function serverState() {
  return {
    globalLamport: syncServer.globalLamport.toString(),
    todos: materializedTodos(),
  };
}

function materializedTodos() {
  const todos = new Map();
  for (const [key, column] of syncServer.cells.entries()) {
    const [todoId, columnName] = splitCellKey(key);
    const current = todos.get(todoId) ?? {
      id: todoId,
      title: "",
      completed: false,
      deleted: false,
    };
    if (columnName === "title" && column.value.type === "text") {
      current.title = column.value.value;
    }
    if (columnName === "completed" && column.value.type === "boolean") {
      current.completed = column.value.value;
    }
    if (columnName === "deleted" && column.value.type === "boolean") {
      current.deleted = column.value.value;
    }
    todos.set(todoId, current);
  }
  return Array.from(todos.values())
    .filter((todo) => !todo.deleted)
    .sort((left, right) => left.id.localeCompare(right.id));
}

function splitCellKey(key) {
  const separator = key.indexOf("|");
  if (separator === -1) {
    throw new Error(`invalid cell key: ${key}`);
  }
  return [
    decodeURIComponent(key.slice(0, separator)),
    decodeURIComponent(key.slice(separator + 1)),
  ];
}

function contentType(filePath) {
  switch (extname(filePath)) {
    case ".html":
      return "text/html; charset=utf-8";
    case ".css":
      return "text/css; charset=utf-8";
    case ".js":
    case ".mjs":
      return "text/javascript; charset=utf-8";
    case ".json":
      return "application/json; charset=utf-8";
    case ".png":
      return "image/png";
    default:
      return "application/octet-stream";
  }
}

function lanAddresses() {
  const addresses = [];
  for (const interfaces of Object.values(networkInterfaces())) {
    for (const details of interfaces ?? []) {
      if (details.family === "IPv4" && !details.internal) {
        addresses.push(details.address);
      }
    }
  }
  return addresses;
}
