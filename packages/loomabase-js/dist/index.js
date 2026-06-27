export const PROTOCOL_VERSION = 4;
export const SERVER_DEVICE_ID = "loomabase-server";
export const DELETED_COLUMN = "deleted";
export const TODOS_TABLE = Object.freeze({
    name: "todos",
    columns: Object.freeze([
        Object.freeze({ name: "title", type: "text" }),
        Object.freeze({ name: "completed", type: "boolean" }),
    ]),
});
const BIGINT_JSON_KEYS = new Set([
    "schema_fingerprint",
    "source_lamport",
    "lamport_clock",
    "lamport",
    "cursor",
    "scope_version",
]);
const BIGINT_MARKER_PREFIX = "__loomabase_bigint__";
const BIGINT_MARKER_SUFFIX = "__";
export function text(value) {
    return { type: "text", value };
}
export function booleanValue(value) {
    return { type: "boolean", value };
}
export function integer(value) {
    assertSafeInteger("integer value", value);
    return { type: "integer", value };
}
export function real(value) {
    if (!Number.isFinite(value)) {
        throw new LoomabaseClientError("real value must be finite");
    }
    return { type: "real", value };
}
export function nullValue() {
    return { type: "null" };
}
export function blob(value) {
    return { type: "blob", value: Array.from(value) };
}
export function fingerprintContract(contract) {
    let descriptor = `loomabase-contract-v1;${contract.name}`;
    for (const column of synchronizedColumns(contract)) {
        descriptor += `;${column.name}:${column.type}`;
    }
    return fnv1a64(new TextEncoder().encode(descriptor));
}
export function emptyPayload(sourceDeviceId, sourceLamport = 0n, table = TODOS_TABLE) {
    validateIdentifier("source_device_id", sourceDeviceId);
    return {
        protocol_version: PROTOCOL_VERSION,
        schema_fingerprint: fingerprintContract(table),
        source_device_id: sourceDeviceId,
        source_lamport: toBigInt(sourceLamport),
        changes: [],
        cursor: 0n,
        has_more: false,
        cursor_reset: false,
        cursor_token: null,
        server_epoch: null,
        rejections: [],
    };
}
export function makeColumn(value, lamportClock, deviceId) {
    validateIdentifier("device_id", deviceId);
    return {
        value,
        metadata: {
            lamport_clock: toBigInt(lamportClock),
            device_id: deviceId,
        },
    };
}
export function compareMetadata(current, incoming) {
    if (incoming.lamport_clock > current.lamport_clock)
        return 1;
    if (incoming.lamport_clock < current.lamport_clock)
        return -1;
    if (incoming.device_id > current.device_id)
        return 1;
    if (incoming.device_id < current.device_id)
        return -1;
    return 0;
}
export function decideLww(current, incoming) {
    const comparison = compareMetadata(current, incoming);
    if (comparison > 0)
        return "accept_incoming";
    if (comparison < 0)
        return "keep_current";
    return "equal";
}
export function explainLww(current, incoming) {
    if (!current) {
        return {
            winner: "incoming",
            reason: "missing_current_value",
            incoming: incoming.metadata,
            summary: "incoming value wins because the cell does not exist",
        };
    }
    const decision = decideLww(current.metadata, incoming.metadata);
    if (decision === "accept_incoming") {
        if (incoming.metadata.lamport_clock > current.metadata.lamport_clock) {
            return {
                winner: "incoming",
                reason: "higher_lamport_clock",
                current: current.metadata,
                incoming: incoming.metadata,
                summary: `incoming clock ${incoming.metadata.lamport_clock} is greater than current clock ${current.metadata.lamport_clock}`,
            };
        }
        return {
            winner: "incoming",
            reason: "device_id_tie_break",
            current: current.metadata,
            incoming: incoming.metadata,
            summary: `clocks are equal and incoming device ID ${JSON.stringify(incoming.metadata.device_id)} sorts after current device ID ${JSON.stringify(current.metadata.device_id)}`,
        };
    }
    if (decision === "keep_current") {
        if (incoming.metadata.lamport_clock < current.metadata.lamport_clock) {
            return {
                winner: "current",
                reason: "lower_lamport_clock",
                current: current.metadata,
                incoming: incoming.metadata,
                summary: `incoming clock ${incoming.metadata.lamport_clock} is lower than current clock ${current.metadata.lamport_clock}`,
            };
        }
        return {
            winner: "current",
            reason: "device_id_tie_break",
            current: current.metadata,
            incoming: incoming.metadata,
            summary: `clocks are equal and incoming device ID ${JSON.stringify(incoming.metadata.device_id)} sorts before current device ID ${JSON.stringify(current.metadata.device_id)}`,
        };
    }
    if (sameCrdtValue(current.value, incoming.value)) {
        return {
            winner: "equal",
            reason: "same_version_same_value",
            current: current.metadata,
            incoming: incoming.metadata,
            summary: "incoming value is an idempotent replay of the current version",
        };
    }
    return {
        winner: "invalid_conflict",
        reason: "same_version_different_value",
        current: current.metadata,
        incoming: incoming.metadata,
        summary: "the same CRDT version identifies two different values",
    };
}
export function stringifySyncPayload(value) {
    const encoded = JSON.stringify(value, (_key, fieldValue) => {
        if (typeof fieldValue === "bigint") {
            return `${BIGINT_MARKER_PREFIX}${fieldValue.toString()}${BIGINT_MARKER_SUFFIX}`;
        }
        return fieldValue;
    });
    if (encoded === undefined) {
        throw new LoomabaseClientError("value cannot be serialized as JSON");
    }
    return encoded.replace(new RegExp(`"${BIGINT_MARKER_PREFIX}(-?\\d+)${BIGINT_MARKER_SUFFIX}"`, "g"), "$1");
}
export function parseSyncPayload(json) {
    return parseWireJson(json);
}
export function parsePartialReplicaResponse(json) {
    return parseWireJson(json);
}
export function parseWireJson(json) {
    const rewritten = json.replace(/"(schema_fingerprint|source_lamport|lamport_clock|lamport|cursor|scope_version)"\s*:\s*(-?\d+)/g, '"$1":"$2"');
    return JSON.parse(rewritten, (key, value) => {
        if (BIGINT_JSON_KEYS.has(key) && typeof value === "string") {
            return BigInt(value);
        }
        return value;
    });
}
export function stringifyReplicaSnapshot(snapshot) {
    return stringifySyncPayload(snapshot);
}
export function parseReplicaSnapshot(json) {
    const snapshot = parseWireJson(json);
    validateSnapshot(snapshot);
    return snapshot;
}
export class LoomabaseHttpClient {
    endpoint;
    token;
    getToken;
    fetchImpl;
    headers;
    constructor(options) {
        this.endpoint = new URL(options.endpoint.toString());
        this.token = options.token;
        this.getToken = options.getToken;
        this.fetchImpl = resolveFetch(options.fetch);
        this.headers = options.headers ?? {};
    }
    async sync(payload) {
        const response = await this.postJson(this.endpoint, payload);
        return parseSyncPayload(response);
    }
    async syncPartial(request) {
        const partialEndpoint = new URL(this.endpoint);
        if (!partialEndpoint.pathname.endsWith("/partial")) {
            partialEndpoint.pathname = `${partialEndpoint.pathname.replace(/\/$/, "")}/partial`;
        }
        const response = await this.postJson(partialEndpoint, request);
        return parsePartialReplicaResponse(response);
    }
    async postJson(url, body) {
        const token = this.getToken ? await this.getToken() : this.token;
        const headers = {
            "content-type": "application/json",
            ...this.headers,
        };
        if (token) {
            headers.authorization = `Bearer ${token}`;
        }
        const response = await this.fetchImpl(url, {
            method: "POST",
            headers,
            body: stringifySyncPayload(body),
        });
        const textBody = await response.text();
        if (!response.ok) {
            throw new LoomabaseHttpError(response.status, textBody);
        }
        return textBody;
    }
}
export class ReferenceSyncServer {
    table;
    globalLamport = 0n;
    cells = new Map();
    seq = 0n;
    cellSeq = new Map();
    constructor(options = {}) {
        this.table = options.table ?? TODOS_TABLE;
    }
    merge(payload, authenticatedDeviceId = payload.source_device_id) {
        validateClientPayload(payload, authenticatedDeviceId, this.table);
        const observed = maxObservedClock(payload);
        const responseRows = new Map();
        for (const row of payload.changes) {
            for (const [columnName, incoming] of Object.entries(row.columns)) {
                const key = cellKey(row.todo_id, columnName);
                const current = this.cells.get(key);
                const explanation = explainLww(current, incoming);
                if (explanation.winner === "invalid_conflict") {
                    throw new LoomabaseClientError(explanation.summary);
                }
                if (explanation.winner === "incoming") {
                    const cloned = cloneColumn(incoming);
                    this.cells.set(key, cloned);
                    this.seq += 1n;
                    this.cellSeq.set(key, this.seq);
                }
            }
        }
        this.globalLamport = maxBigInt(this.globalLamport, observed) + 1n;
        const effectiveCursor = payload.cursor;
        const ordered = Array.from(this.cells.entries())
            .map(([key, column]) => ({ key, column, seq: this.cellSeq.get(key) ?? 0n }))
            .filter((entry) => entry.seq > effectiveCursor)
            .sort((left, right) => compareBigInt(left.seq, right.seq));
        let nextCursor = effectiveCursor;
        for (const entry of ordered) {
            const [todoId, columnName] = splitCellKey(entry.key);
            const row = responseRows.get(todoId) ?? {};
            row[columnName] = cloneColumn(entry.column);
            responseRows.set(todoId, row);
            nextCursor = entry.seq;
        }
        if (ordered.length === 0) {
            nextCursor = this.seq;
        }
        return {
            protocol_version: payload.protocol_version,
            schema_fingerprint: fingerprintContract(this.table),
            source_device_id: SERVER_DEVICE_ID,
            source_lamport: this.globalLamport,
            changes: Array.from(responseRows.entries()).map(([todo_id, columns]) => ({
                todo_id,
                columns,
            })),
            cursor: nextCursor,
            has_more: false,
            cursor_reset: false,
            cursor_token: null,
            server_epoch: null,
            rejections: [],
        };
    }
}
export class LocalStorageTodoReplicaStorage {
    key;
    storage;
    constructor(options) {
        validateIdentifier("localStorage key", options.key);
        const storage = options.storage ?? globalThis.localStorage;
        if (!storage) {
            throw new LoomabaseClientError("localStorage is not available; pass options.storage");
        }
        this.key = options.key;
        this.storage = storage;
    }
    async load() {
        const value = this.storage.getItem(this.key);
        return value ? parseReplicaSnapshot(value) : null;
    }
    async save(snapshot) {
        this.storage.setItem(this.key, stringifyReplicaSnapshot(snapshot));
    }
    async clear() {
        this.storage.removeItem(this.key);
    }
}
export class JsonFileTodoReplicaStorage {
    path;
    constructor(path) {
        if (!path) {
            throw new LoomabaseClientError("snapshot path cannot be empty");
        }
        this.path = path;
    }
    async load() {
        const fs = await nodeFsPromises();
        try {
            return parseReplicaSnapshot(await fs.readFile(this.path, "utf8"));
        }
        catch (error) {
            if (isNodeNotFoundError(error)) {
                return null;
            }
            throw error;
        }
    }
    async save(snapshot) {
        const fs = await nodeFsPromises();
        const parent = dirname(this.path);
        if (parent) {
            await fs.mkdir(parent, { recursive: true });
        }
        const tmpPath = `${this.path}.${Date.now()}.${Math.random().toString(16).slice(2)}.tmp`;
        await fs.writeFile(tmpPath, stringifyReplicaSnapshot(snapshot), "utf8");
        await fs.rename(tmpPath, this.path);
    }
    async clear() {
        const fs = await nodeFsPromises();
        try {
            await fs.unlink(this.path);
        }
        catch (error) {
            if (!isNodeNotFoundError(error)) {
                throw error;
            }
        }
    }
}
export class MemoryTodoReplica {
    deviceId;
    table;
    lamport = 0n;
    cursor = 0n;
    todos = new Map();
    cells = new Map();
    dirty = new Set();
    constructor(options) {
        validateIdentifier("device_id", options.deviceId);
        this.deviceId = options.deviceId;
        this.table = options.table ?? TODOS_TABLE;
    }
    static fromSnapshot(snapshot, options = {}) {
        validateSnapshot(snapshot);
        const table = options.table ?? snapshot.table;
        const deviceId = options.deviceId ?? snapshot.device_id;
        const replica = new MemoryTodoReplica({ deviceId, table });
        if (fingerprintContract(replica.table) !== fingerprintContract(snapshot.table)) {
            throw new LoomabaseClientError("snapshot schema fingerprint does not match replica table");
        }
        replica.lamport = snapshot.lamport;
        replica.cursor = snapshot.cursor;
        replica.todos = new Map(snapshot.todos.map((todo) => [todo.id, { ...todo }]));
        replica.cells = new Map(snapshot.cells.map(([key, column]) => [key, cloneColumn(column)]));
        replica.dirty = new Set(snapshot.dirty);
        return replica;
    }
    static async open(options) {
        const snapshot = await options.storage.load();
        const replicaOptions = options.table
            ? { deviceId: options.deviceId, table: options.table }
            : { deviceId: options.deviceId };
        if (!snapshot) {
            return new MemoryTodoReplica(replicaOptions);
        }
        return MemoryTodoReplica.fromSnapshot(snapshot, replicaOptions);
    }
    snapshot() {
        return {
            device_id: this.deviceId,
            table: cloneTable(this.table),
            lamport: this.lamport,
            cursor: this.cursor,
            todos: Array.from(this.todos.values()).map((todo) => ({ ...todo })),
            cells: Array.from(this.cells.entries()).map(([key, column]) => [
                key,
                cloneColumn(column),
            ]),
            dirty: Array.from(this.dirty).sort(),
        };
    }
    async save(storage) {
        await storage.save(this.snapshot());
    }
    createTodo(id, titleValue, completed = false) {
        validateIdentifier("todo_id", id);
        this.writeCell(id, "title", text(titleValue));
        this.writeCell(id, "completed", booleanValue(completed));
        this.writeCell(id, DELETED_COLUMN, booleanValue(false));
    }
    updateTitle(id, titleValue) {
        validateIdentifier("todo_id", id);
        this.writeCell(id, "title", text(titleValue));
    }
    setCompleted(id, completed) {
        validateIdentifier("todo_id", id);
        this.writeCell(id, "completed", booleanValue(completed));
    }
    deleteTodo(id) {
        validateIdentifier("todo_id", id);
        this.writeCell(id, DELETED_COLUMN, booleanValue(true));
    }
    restoreTodo(id) {
        validateIdentifier("todo_id", id);
        this.writeCell(id, DELETED_COLUMN, booleanValue(false));
    }
    getTodo(id) {
        const todo = this.todos.get(id);
        if (!todo || todo.deleted)
            return undefined;
        return { ...todo };
    }
    listTodos() {
        return Array.from(this.todos.values())
            .filter((todo) => !todo.deleted)
            .map((todo) => ({ ...todo }))
            .sort((left, right) => left.id.localeCompare(right.id));
    }
    getCell(id, columnName) {
        const column = this.cells.get(cellKey(id, columnName));
        return column ? cloneColumn(column) : undefined;
    }
    localDelta() {
        const rows = new Map();
        for (const key of Array.from(this.dirty).sort()) {
            const [todoId, columnName] = splitCellKey(key);
            const column = this.cells.get(key);
            if (!column)
                continue;
            const row = rows.get(todoId) ?? {};
            row[columnName] = cloneColumn(column);
            rows.set(todoId, row);
        }
        return {
            protocol_version: PROTOCOL_VERSION,
            schema_fingerprint: fingerprintContract(this.table),
            source_device_id: this.deviceId,
            source_lamport: this.lamport,
            changes: Array.from(rows.entries()).map(([todo_id, columns]) => ({
                todo_id,
                columns,
            })),
            cursor: this.cursor,
            has_more: false,
            cursor_reset: false,
            cursor_token: null,
            server_epoch: null,
            rejections: [],
        };
    }
    acknowledge(sent, response) {
        validateClientPayload(sent, this.deviceId, this.table);
        const rejected = new Set((response?.rejections ?? []).map((rejection) => versionKey(rejection.todo_id, rejection.column_name, rejection.metadata.lamport_clock, rejection.metadata.device_id)));
        for (const row of sent.changes) {
            for (const [columnName, column] of Object.entries(row.columns)) {
                if (rejected.has(versionKey(row.todo_id, columnName, column.metadata.lamport_clock, column.metadata.device_id))) {
                    continue;
                }
                const key = cellKey(row.todo_id, columnName);
                const current = this.cells.get(key);
                if (current &&
                    current.metadata.lamport_clock === column.metadata.lamport_clock &&
                    current.metadata.device_id === column.metadata.device_id) {
                    this.dirty.delete(key);
                }
            }
        }
    }
    applyRemote(response) {
        validateServerPayload(response, this.table);
        const explanations = [];
        for (const row of response.changes) {
            for (const [columnName, incoming] of Object.entries(row.columns)) {
                const key = cellKey(row.todo_id, columnName);
                const current = this.cells.get(key);
                const explanation = explainLww(current, incoming);
                explanations.push(explanation);
                if (explanation.winner === "invalid_conflict") {
                    throw new LoomabaseClientError(explanation.summary);
                }
                if (explanation.winner === "incoming") {
                    this.cells.set(key, cloneColumn(incoming));
                    this.materialize(row.todo_id);
                }
            }
        }
        this.lamport = maxBigInt(this.lamport, maxObservedClock(response)) + 1n;
        this.cursor = response.cursor_reset ? response.cursor : maxBigInt(this.cursor, response.cursor);
        return explanations;
    }
    completeSync(sent, response) {
        this.acknowledge(sent, response);
        return this.applyRemote(response);
    }
    async syncWith(transport) {
        const outbound = this.localDelta();
        const response = await transport(outbound);
        this.completeSync(outbound, response);
        return response;
    }
    writeCell(id, columnName, value) {
        validateColumn(this.table, columnName, value);
        this.lamport += 1n;
        const column = makeColumn(value, this.lamport, this.deviceId);
        const key = cellKey(id, columnName);
        this.cells.set(key, column);
        this.dirty.add(key);
        this.materialize(id);
    }
    materialize(id) {
        const existing = this.todos.get(id) ?? {
            id,
            title: "",
            completed: false,
            deleted: false,
        };
        const titleCell = this.cells.get(cellKey(id, "title"));
        const completedCell = this.cells.get(cellKey(id, "completed"));
        const deletedCell = this.cells.get(cellKey(id, DELETED_COLUMN));
        const titleValue = titleCell?.value;
        const completedValue = completedCell?.value;
        const deletedValue = deletedCell?.value;
        this.todos.set(id, {
            id,
            title: titleValue?.type === "text" ? titleValue.value : existing.title,
            completed: completedValue?.type === "boolean" ? completedValue.value : existing.completed,
            deleted: deletedValue?.type === "boolean" ? deletedValue.value : existing.deleted,
        });
    }
}
export class LoomabaseClientError extends Error {
    constructor(message) {
        super(message);
        this.name = "LoomabaseClientError";
    }
}
export class LoomabaseHttpError extends LoomabaseClientError {
    status;
    body;
    constructor(status, body) {
        super(`Loomabase HTTP request failed with status ${status}: ${body}`);
        this.name = "LoomabaseHttpError";
        this.status = status;
        this.body = body;
    }
}
function validateClientPayload(payload, authenticatedDeviceId, table) {
    validatePayload(payload, table);
    if (payload.source_device_id !== authenticatedDeviceId) {
        throw new LoomabaseClientError("source_device_id does not match the authenticated device");
    }
    for (const row of payload.changes) {
        for (const column of Object.values(row.columns)) {
            if (column.metadata.device_id !== authenticatedDeviceId) {
                throw new LoomabaseClientError("a client cannot attribute a change to another device");
            }
        }
    }
}
function validateServerPayload(payload, table) {
    validatePayload(payload, table);
    if (payload.source_device_id !== SERVER_DEVICE_ID) {
        throw new LoomabaseClientError("remote payload is not attributed to Loomabase server");
    }
}
function validatePayload(payload, table) {
    if (payload.protocol_version !== PROTOCOL_VERSION) {
        throw new LoomabaseClientError(`unsupported protocol version ${payload.protocol_version}`);
    }
    if (payload.schema_fingerprint !== fingerprintContract(table)) {
        throw new LoomabaseClientError("schema fingerprint mismatch");
    }
    validateIdentifier("source_device_id", payload.source_device_id);
    for (const row of payload.changes) {
        validateIdentifier("todo_id", row.todo_id);
        for (const [columnName, column] of Object.entries(row.columns)) {
            validateColumn(table, columnName, column.value);
            validateIdentifier("metadata.device_id", column.metadata.device_id);
        }
    }
}
function validateSnapshot(snapshot) {
    validateIdentifier("snapshot.device_id", snapshot.device_id);
    validateIdentifier("snapshot.table.name", snapshot.table.name);
    for (const column of snapshot.table.columns) {
        validateIdentifier("snapshot.column.name", column.name);
    }
    for (const todo of snapshot.todos) {
        validateIdentifier("snapshot.todo.id", todo.id);
    }
    for (const [key, column] of snapshot.cells) {
        splitCellKey(key);
        validateIdentifier("snapshot.cell.device_id", column.metadata.device_id);
    }
    for (const key of snapshot.dirty) {
        splitCellKey(key);
    }
}
function validateColumn(table, columnName, value) {
    const column = synchronizedColumns(table).find((candidate) => candidate.name === columnName);
    if (!column) {
        throw new LoomabaseClientError(`column is not synchronizable: ${columnName}`);
    }
    switch (column.type) {
        case "text":
            if (value.type !== "text") {
                throw new LoomabaseClientError(`invalid value type for ${columnName}`);
            }
            return;
        case "integer":
            if (value.type !== "integer") {
                throw new LoomabaseClientError(`invalid value type for ${columnName}`);
            }
            assertSafeInteger(columnName, value.value);
            return;
        case "real":
            if (value.type !== "real" || !Number.isFinite(value.value)) {
                throw new LoomabaseClientError(`invalid value type for ${columnName}`);
            }
            return;
        case "boolean":
            if (value.type !== "boolean") {
                throw new LoomabaseClientError(`invalid value type for ${columnName}`);
            }
            return;
    }
}
function synchronizedColumns(table) {
    return [...table.columns, { name: DELETED_COLUMN, type: "boolean" }];
}
function fnv1a64(bytes) {
    let hash = 0xcbf29ce484222325n;
    for (const byte of bytes) {
        hash ^= BigInt(byte);
        hash = BigInt.asUintN(64, hash * 0x100000001b3n);
    }
    return hash;
}
function maxObservedClock(payload) {
    let max = payload.source_lamport;
    for (const row of payload.changes) {
        for (const column of Object.values(row.columns)) {
            max = maxBigInt(max, column.metadata.lamport_clock);
        }
    }
    return max;
}
function sameCrdtValue(left, right) {
    return JSON.stringify(left) === JSON.stringify(right);
}
function cloneColumn(column) {
    return {
        value: structuredCloneSafe(column.value),
        metadata: { ...column.metadata },
    };
}
function cloneTable(table) {
    return {
        name: table.name,
        columns: table.columns.map((column) => ({ ...column })),
    };
}
function structuredCloneSafe(value) {
    return globalThis.structuredClone
        ? globalThis.structuredClone(value)
        : JSON.parse(JSON.stringify(value));
}
function cellKey(todoId, columnName) {
    return `${encodeURIComponent(todoId)}|${encodeURIComponent(columnName)}`;
}
function splitCellKey(key) {
    const separator = key.indexOf("|");
    if (separator === -1)
        throw new LoomabaseClientError("invalid cell key");
    return [
        decodeURIComponent(key.slice(0, separator)),
        decodeURIComponent(key.slice(separator + 1)),
    ];
}
function versionKey(todoId, columnName, lamportClock, deviceId) {
    return `${cellKey(todoId, columnName)}|${lamportClock}|${encodeURIComponent(deviceId)}`;
}
function toBigInt(value) {
    if (typeof value === "bigint")
        return value;
    assertSafeInteger("counter", value);
    return BigInt(value);
}
function maxBigInt(left, right) {
    return left > right ? left : right;
}
function compareBigInt(left, right) {
    if (left < right)
        return -1;
    if (left > right)
        return 1;
    return 0;
}
function assertSafeInteger(name, value) {
    if (!Number.isSafeInteger(value)) {
        throw new LoomabaseClientError(`${name} must be a safe JavaScript integer`);
    }
}
function validateIdentifier(field, value) {
    if (!value || value.length > 255 || /[\u0000-\u001f\u007f]/u.test(value)) {
        throw new LoomabaseClientError(`${field} must contain between 1 and 255 non-control characters`);
    }
}
async function nodeFsPromises() {
    const importer = new Function("specifier", "return import(specifier)");
    return importer("node:fs/promises");
}
function resolveFetch(fetchOverride) {
    const resolved = fetchOverride ?? globalThis.fetch;
    if (!resolved) {
        throw new LoomabaseClientError("fetch is not available; pass options.fetch");
    }
    return resolved.bind(globalThis);
}
function isNodeNotFoundError(error) {
    return (typeof error === "object" &&
        error !== null &&
        "code" in error &&
        error.code === "ENOENT");
}
function dirname(path) {
    const normalized = path.replaceAll("\\", "/");
    const index = normalized.lastIndexOf("/");
    if (index <= 0) {
        return index === 0 ? "/" : "";
    }
    return path.slice(0, index);
}
//# sourceMappingURL=index.js.map