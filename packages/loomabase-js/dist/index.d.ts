export type ColumnType = "text" | "integer" | "real" | "boolean";
export interface ColumnDef {
    name: string;
    type: ColumnType;
}
export interface TableContract {
    name: string;
    columns: readonly ColumnDef[];
}
export type CrdtValue = {
    type: "null";
} | {
    type: "integer";
    value: number;
} | {
    type: "real";
    value: number;
} | {
    type: "text";
    value: string;
} | {
    type: "boolean";
    value: boolean;
} | {
    type: "blob";
    value: number[];
};
export interface ColumnMetadata {
    lamport_clock: bigint;
    device_id: string;
}
export interface CrdtColumn {
    value: CrdtValue;
    metadata: ColumnMetadata;
}
export interface RowChange {
    todo_id: string;
    columns: Record<string, CrdtColumn>;
}
export type SyncRejectionKind = "authorization_denied" | "validation_failed";
export interface SyncRejection {
    todo_id: string;
    column_name: string;
    kind: SyncRejectionKind;
    reason: string;
    value: CrdtValue;
    metadata: ColumnMetadata;
}
export interface SyncPayload {
    protocol_version: number;
    schema_fingerprint: bigint;
    source_device_id: string;
    source_lamport: bigint;
    changes: RowChange[];
    cursor: bigint;
    has_more?: boolean;
    cursor_reset?: boolean;
    cursor_token?: string | null;
    server_epoch?: string | null;
    rejections?: SyncRejection[];
}
export interface PartialReplicaRequest {
    scope_id: string;
    scope_version: bigint;
    interest: ReplicaInterest;
    known_member_ids: string[];
    sync: SyncPayload;
}
export interface PartialReplicaResponse {
    scope_id: string;
    scope_version: bigint;
    member_ids: string[];
    evicted_row_ids: string[];
    sync: SyncPayload;
}
export interface ReplicaInterest {
    predicates: ReplicaPredicate[];
    limit: number;
}
export type ReplicaPredicate = {
    kind: "id_equals";
    value: string;
} | {
    kind: "id_prefix";
    value: string;
} | {
    kind: "column_equals";
    value: {
        column: string;
        value: CrdtValue;
    };
};
export interface Todo {
    id: string;
    title: string;
    completed: boolean;
    deleted: boolean;
}
export interface TodoReplicaSnapshot {
    device_id: string;
    table: TableContract;
    lamport: bigint;
    cursor: bigint;
    todos: Todo[];
    cells: Array<[string, CrdtColumn]>;
    dirty: string[];
}
export interface TodoReplicaStorage {
    load(): Promise<TodoReplicaSnapshot | null>;
    save(snapshot: TodoReplicaSnapshot): Promise<void>;
    clear?(): Promise<void>;
}
export type MergeDecision = "accept_incoming" | "keep_current" | "equal";
export interface ConflictExplanation {
    winner: "incoming" | "current" | "equal" | "invalid_conflict";
    reason: "missing_current_value" | "higher_lamport_clock" | "lower_lamport_clock" | "device_id_tie_break" | "same_version_same_value" | "same_version_different_value";
    summary: string;
    current?: ColumnMetadata;
    incoming: ColumnMetadata;
}
export declare const PROTOCOL_VERSION = 4;
export declare const SERVER_DEVICE_ID = "loomabase-server";
export declare const DELETED_COLUMN = "deleted";
export declare const TODOS_TABLE: TableContract;
export declare function text(value: string): CrdtValue;
export declare function booleanValue(value: boolean): CrdtValue;
export declare function integer(value: number): CrdtValue;
export declare function real(value: number): CrdtValue;
export declare function nullValue(): CrdtValue;
export declare function blob(value: Uint8Array | number[]): CrdtValue;
export declare function fingerprintContract(contract: TableContract): bigint;
export declare function emptyPayload(sourceDeviceId: string, sourceLamport?: bigint | number, table?: TableContract): SyncPayload;
export declare function makeColumn(value: CrdtValue, lamportClock: bigint | number, deviceId: string): CrdtColumn;
export declare function compareMetadata(current: ColumnMetadata, incoming: ColumnMetadata): number;
export declare function decideLww(current: ColumnMetadata, incoming: ColumnMetadata): MergeDecision;
export declare function explainLww(current: CrdtColumn | undefined, incoming: CrdtColumn): ConflictExplanation;
export declare function stringifySyncPayload(value: unknown): string;
export declare function parseSyncPayload(json: string): SyncPayload;
export declare function parsePartialReplicaResponse(json: string): PartialReplicaResponse;
export declare function parseWireJson(json: string): unknown;
export declare function stringifyReplicaSnapshot(snapshot: TodoReplicaSnapshot): string;
export declare function parseReplicaSnapshot(json: string): TodoReplicaSnapshot;
export interface LoomabaseHttpClientOptions {
    endpoint: string | URL;
    token?: string;
    getToken?: () => Promise<string | null | undefined> | string | null | undefined;
    fetch?: typeof fetch;
    headers?: Record<string, string>;
}
export declare class LoomabaseHttpClient {
    readonly endpoint: URL;
    private readonly token;
    private readonly getToken;
    private readonly fetchImpl;
    private readonly headers;
    constructor(options: LoomabaseHttpClientOptions);
    sync(payload: SyncPayload): Promise<SyncPayload>;
    syncPartial(request: PartialReplicaRequest): Promise<PartialReplicaResponse>;
    private postJson;
}
export declare class ReferenceSyncServer {
    readonly table: TableContract;
    globalLamport: bigint;
    cells: Map<string, CrdtColumn>;
    private seq;
    private cellSeq;
    constructor(options?: {
        table?: TableContract;
    });
    merge(payload: SyncPayload, authenticatedDeviceId?: string): SyncPayload;
}
export declare class LocalStorageTodoReplicaStorage implements TodoReplicaStorage {
    private readonly key;
    private readonly storage;
    constructor(options: {
        key: string;
        storage?: Storage;
    });
    load(): Promise<TodoReplicaSnapshot | null>;
    save(snapshot: TodoReplicaSnapshot): Promise<void>;
    clear(): Promise<void>;
}
export declare class JsonFileTodoReplicaStorage implements TodoReplicaStorage {
    private readonly path;
    constructor(path: string);
    load(): Promise<TodoReplicaSnapshot | null>;
    save(snapshot: TodoReplicaSnapshot): Promise<void>;
    clear(): Promise<void>;
}
export declare class MemoryTodoReplica {
    readonly deviceId: string;
    readonly table: TableContract;
    lamport: bigint;
    cursor: bigint;
    todos: Map<string, Todo>;
    private cells;
    private dirty;
    constructor(options: {
        deviceId: string;
        table?: TableContract;
    });
    static fromSnapshot(snapshot: TodoReplicaSnapshot, options?: {
        deviceId?: string;
        table?: TableContract;
    }): MemoryTodoReplica;
    static open(options: {
        deviceId: string;
        storage: TodoReplicaStorage;
        table?: TableContract;
    }): Promise<MemoryTodoReplica>;
    snapshot(): TodoReplicaSnapshot;
    save(storage: TodoReplicaStorage): Promise<void>;
    createTodo(id: string, titleValue: string, completed?: boolean): void;
    updateTitle(id: string, titleValue: string): void;
    setCompleted(id: string, completed: boolean): void;
    deleteTodo(id: string): void;
    restoreTodo(id: string): void;
    getTodo(id: string): Todo | undefined;
    listTodos(): Todo[];
    getCell(id: string, columnName: string): CrdtColumn | undefined;
    localDelta(): SyncPayload;
    acknowledge(sent: SyncPayload, response?: SyncPayload): void;
    applyRemote(response: SyncPayload): ConflictExplanation[];
    completeSync(sent: SyncPayload, response: SyncPayload): ConflictExplanation[];
    syncWith(transport: (payload: SyncPayload) => Promise<SyncPayload>): Promise<SyncPayload>;
    private writeCell;
    private materialize;
}
export declare class LoomabaseClientError extends Error {
    constructor(message: string);
}
export declare class LoomabaseHttpError extends LoomabaseClientError {
    readonly status: number;
    readonly body: string;
    constructor(status: number, body: string);
}
