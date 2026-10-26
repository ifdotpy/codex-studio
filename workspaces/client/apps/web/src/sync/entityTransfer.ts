import { subscriptionKey } from "./subscriptions/contracts";
import { SYNC_PROTOCOL_VERSION, type components } from "../generated/api";
import {
  MAX_SYNC_RESOURCE_COUNT,
  SYNC_ENTITY_TRANSFER_ID_MAX_CODEPOINTS,
  SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
  SYNC_STREAM_EVENT_MAX_BYTES,
} from "../generated/sharedValues";
import { SUBSCRIPTION_ENTITY_SCOPES } from "../generated/subscriptionEntities";

type Batch = components["schemas"]["EntityChangeBatch"];
type Frame = components["schemas"]["ResourceEntityTransferEvent"];
export type StagedEntityInterval = Omit<
  Frame,
  "data" | "partIndex" | "partCount" | "complete"
> & {
  after: number;
  pages(): AsyncIterable<Batch>;
  has(id: string): Promise<boolean>;
  dispose(): Promise<void>;
};
export interface TransferStorage {
  put(key: string, value: string): Promise<void>;
  putMany(values: [string, string][]): Promise<void>;
  get(key: string): Promise<string | undefined>;
  remove(prefix: string): Promise<void>;
  close(): Promise<void>;
}
const STAGING_PREFIX = "studio-entity-transfer-";

/** Each owner has an unguessable database identity. Cancellation deletes only
 * this owner's database, never another tab's active or recovering transfer. */
export class IndexedTransferStorage implements TransferStorage {
  private readonly name = `${STAGING_PREFIX}${crypto.randomUUID()}`;
  private database: Promise<IDBDatabase> | undefined;
  private stopped = false;
  private releaseLease: (() => void) | undefined;
  private readonly onPageHide = () => {
    void this.close().catch((error) =>
      console.error("Entity staging cleanup failed", error),
    );
  };
  constructor() {
    globalThis.addEventListener?.("pagehide", this.onPageHide, { once: true });
  }
  private async acquireLease() {
    if (!navigator.locks?.request)
      throw new Error("Entity staging owner lock unavailable");
    let acquired!: () => void;
    let reject!: (error: unknown) => void;
    const ready = new Promise<void>((resolve, failed) => {
      acquired = resolve;
      reject = failed;
    });
    const lifetime = new Promise<void>((resolve) => {
      this.releaseLease = resolve;
    });
    void navigator.locks
      .request(this.name, async () => {
        acquired();
        await lifetime;
      })
      .catch(reject);
    await ready;
    if (this.stopped) {
      this.releaseLease?.();
      throw new Error("Entity staging closed");
    }
    // Once per owner, after acquiring its lease. A crash releases the old lock;
    // live tabs keep theirs and cannot have their staging removed here.
    await this.cleanOrphans();
  }
  private async cleanOrphans() {
    if (!indexedDB.databases) return;
    for (const { name } of await indexedDB.databases()) {
      if (!name?.startsWith(STAGING_PREFIX) || name === this.name) continue;
      await navigator.locks.request(
        name,
        { ifAvailable: true },
        async (lock) => {
          if (!lock) return;
          await new Promise<void>((resolve, reject) => {
            const request = indexedDB.deleteDatabase(name);
            request.onsuccess = () => resolve();
            request.onerror = () => reject(request.error);
            request.onblocked = () =>
              reject(new Error("Orphan entity staging cleanup blocked"));
          });
        },
      );
    }
  }
  private open() {
    if (this.stopped) return Promise.reject(new Error("Entity staging closed"));
    this.database ??= this.acquireLease().then(
      () =>
        new Promise<IDBDatabase>((resolve, reject) => {
          const request = indexedDB.open(this.name, 1);
          request.onupgradeneeded = () =>
            request.result.createObjectStore("pages");
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => reject(request.error);
        }),
    );
    return this.database;
  }
  private async transaction<T>(
    mode: IDBTransactionMode,
    run: (store: IDBObjectStore, result: (value: T) => void) => void,
  ): Promise<T> {
    const db = await this.open();
    if (this.stopped) throw new Error("Entity staging closed");
    return new Promise((resolve, reject) => {
      const tx = db.transaction("pages", mode);
      let result: T;
      tx.oncomplete = () => resolve(result);
      tx.onerror = () => reject(tx.error);
      tx.onabort = () =>
        reject(tx.error ?? new Error("Entity staging aborted"));
      run(tx.objectStore("pages"), (value) => {
        result = value;
      });
    });
  }
  put(key: string, value: string) {
    return this.transaction<void>("readwrite", (store) => {
      store.put(value, key);
    });
  }
  putMany(values: [string, string][]) {
    return this.transaction<void>("readwrite", (store) => {
      for (const [key, value] of values) store.put(value, key);
    });
  }
  get(key: string) {
    return this.transaction<string | undefined>("readonly", (store, accept) => {
      const request = store.get(key);
      request.onsuccess = () =>
        accept(typeof request.result === "string" ? request.result : undefined);
    });
  }
  remove(prefix: string) {
    return this.transaction<void>("readwrite", (store) => {
      store.delete(IDBKeyRange.bound(prefix, `${prefix}\uffff`));
    });
  }
  async close() {
    if (this.stopped) return;
    this.stopped = true;
    globalThis.removeEventListener?.("pagehide", this.onPageHide);
    try {
      (await this.database)?.close();
      await new Promise<void>((resolve, reject) => {
        const request = indexedDB.deleteDatabase(this.name);
        request.onsuccess = () => resolve();
        request.onerror = () => reject(request.error);
        request.onblocked = () =>
          reject(new Error("Entity staging cleanup blocked"));
      });
    } finally {
      this.releaseLease?.();
    }
  }
}
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function sequence(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
const frameFields = new Set([
  "protocol",
  "workspaceId",
  "epoch",
  "revision",
  "reason",
  "resource",
  "cursor",
  "replyKind",
  "transferId",
  "pageIndex",
  "partIndex",
  "partCount",
  "complete",
  "data",
]);
function frameValue(value: unknown): Frame {
  if (
    !object(value) ||
    Object.keys(value).some((key) => !frameFields.has(key)) ||
    Object.keys(value).length !== frameFields.size ||
    value.protocol !== SYNC_PROTOCOL_VERSION ||
    typeof value.workspaceId !== "string" ||
    !value.workspaceId ||
    typeof value.epoch !== "string" ||
    !value.epoch ||
    !sequence(value.revision) ||
    !["initial", "reconnect", "change", "workspace", "overflow"].includes(
      String(value.reason),
    ) ||
    !object(value.resource) ||
    typeof value.resource.kind !== "string" ||
    !(value.resource.kind in SUBSCRIPTION_ENTITY_SCOPES) ||
    !object(value.cursor) ||
    value.cursor.workspaceId !== value.workspaceId ||
    !sequence(value.cursor.seq) ||
    Object.keys(value.cursor).some(
      (key) => key !== "workspaceId" && key !== "seq",
    ) ||
    !["snapshot", "delta"].includes(String(value.replyKind)) ||
    typeof value.transferId !== "string" ||
    !value.transferId ||
    Array.from(value.transferId).length >
      SYNC_ENTITY_TRANSFER_ID_MAX_CODEPOINTS ||
    !sequence(value.pageIndex) ||
    !sequence(value.partIndex) ||
    !sequence(value.partCount) ||
    value.partCount < 1 ||
    value.partIndex >= value.partCount ||
    typeof value.complete !== "boolean" ||
    (value.complete && value.partIndex + 1 !== value.partCount) ||
    typeof value.data !== "string" ||
    new TextEncoder().encode(
      `event: entity-transfer\ndata: ${JSON.stringify(value)}\n\n`,
    ).length > SYNC_STREAM_EVENT_MAX_BYTES
  )
    throw new TypeError("Invalid bounded entity transfer frame");
  return value as Frame;
}
/** Canonical values were validated by the server Registry before framing. Keep
 * the existing entity DTO contract, and check transport/page interval boundaries
 * before any cache write, without introducing another schema implementation. */
function pageValue(value: unknown, after: number, through: number): Batch {
  if (
    !object(value) ||
    Object.keys(value).some(
      (key) => !["after", "through", "documents"].includes(key),
    ) ||
    value.after !== after ||
    !sequence(value.through) ||
    value.through < after ||
    value.through > through ||
    !Array.isArray(value.documents) ||
    value.documents.length > SYNC_ENTITY_STREAM_MAX_DOCUMENTS
  )
    throw new TypeError("Invalid canonical entity page interval");
  let previous = after;
  for (const row of value.documents) {
    if (
      !object(row) ||
      !sequence(row.seq) ||
      row.seq <= previous ||
      row.seq > value.through ||
      typeof row.id !== "string" ||
      typeof row.collection !== "string" ||
      !["upsert", "delete"].includes(String(row.op)) ||
      (row.op === "upsert" ? !object(row.value) : "value" in row) ||
      Object.keys(row).some(
        (key) => !["op", "collection", "id", "seq", "value"].includes(key),
      )
    )
      throw new TypeError("Invalid canonical entity page record");
    previous = row.seq;
  }
  return value as Batch;
}
type Pending = {
  frame: Frame;
  page: number;
  part: number;
  after: number | undefined;
  through: number | undefined;
  complete: boolean;
};

/** Only interval metadata stays in memory. Serialized fragments and validated
 * pages remain in temporary storage until the complete interval is finalized. */
export class EntityTransferAssembler {
  private readonly pending = new Map<string, Pending>();
  private generation = 0;
  constructor(
    private readonly storage: TransferStorage = new IndexedTransferStorage(),
  ) {}
  async invalidate() {
    this.generation++;
    this.pending.clear();
    await this.storage.close();
  }
  /** Called after queued fragment work settles; unrelated staging stays intact. */
  async remove(resources: components["schemas"]["ResourceRef"][]) {
    const removed = new Set(resources.map(subscriptionKey));
    for (const [scope, pending] of this.pending) {
      if (!removed.has(subscriptionKey(pending.frame.resource))) continue;
      this.pending.delete(scope);
      await this.storage.remove(`${pending.frame.transferId}:`);
    }
  }
  async receive(value: unknown): Promise<StagedEntityInterval | undefined> {
    const frame = frameValue(value);
    const generation = this.generation;
    const scope = JSON.stringify(frame.resource);
    let pending = this.pending.get(scope);
    const prefix = `${frame.transferId}:`;
    const key = `${prefix}fragment:${frame.pageIndex}:${frame.partIndex}`;
    if (
      pending &&
      (pending.frame.workspaceId !== frame.workspaceId ||
        pending.frame.epoch !== frame.epoch ||
        pending.frame.transferId !== frame.transferId)
    ) {
      await this.storage.remove(`${pending.frame.transferId}:`);
      this.pending.delete(scope);
      pending = undefined;
    }
    if (!pending) {
      if (
        frame.pageIndex !== 0 ||
        frame.partIndex !== 0 ||
        this.pending.size >= MAX_SYNC_RESOURCE_COUNT
      )
        throw new Error("Entity transfer has no initial page");
      pending = {
        frame,
        page: 0,
        part: 0,
        after: undefined,
        through: undefined,
        complete: false,
      };
      this.pending.set(scope, pending);
    }
    const first = pending.frame;
    if (
      frame.cursor.seq !== first.cursor.seq ||
      frame.replyKind !== first.replyKind ||
      frame.revision !== first.revision ||
      frame.reason !== first.reason
    )
      throw new Error("Entity transfer identity changed");
    if (
      frame.pageIndex < pending.page ||
      (frame.pageIndex === pending.page && frame.partIndex < pending.part) ||
      pending.complete
    ) {
      if ((await this.storage.get(key)) !== frame.data)
        throw new Error("Conflicting entity transfer replay");
      return;
    }
    if (
      frame.pageIndex !== pending.page ||
      frame.partIndex !== pending.part ||
      (frame.partIndex > 0 && frame.partCount !== pending.frame.partCount)
    )
      throw new Error("Entity transfer part out of order");
    if (frame.partIndex === 0) pending.frame = frame;
    await this.storage.put(key, frame.data);
    if (generation !== this.generation) return;
    pending.part++;
    if (pending.part !== frame.partCount) return;
    const chunks: string[] = [];
    for (let index = 0; index < frame.partCount; index++) {
      const chunk = await this.storage.get(
        `${prefix}fragment:${frame.pageIndex}:${index}`,
      );
      if (chunk === undefined)
        throw new Error("Missing entity transfer fragment");
      chunks.push(chunk);
    }
    const encoded = chunks.join("");
    const decoded: unknown = JSON.parse(encoded);
    const after =
      pending.through ??
      (object(decoded) && sequence(decoded.after) ? decoded.after : -1);
    const batch = pageValue(decoded, after, frame.cursor.seq);
    if (pending.after === undefined) {
      pending.after = batch.after;
      if (frame.replyKind === "snapshot" && batch.after !== 0)
        throw new Error("Snapshot transfer has a nonzero base");
    }
    await this.storage.putMany([
      [`${prefix}page:${frame.pageIndex}`, encoded],
      ...batch.documents.map((row): [string, string] => [
        `${prefix}id:${row.collection}:${row.id}`,
        "1",
      ]),
    ]);
    pending.through = batch.through;
    pending.page++;
    pending.part = 0;
    if (!frame.complete) return;
    if (pending.through !== frame.cursor.seq)
      throw new Error("Incomplete entity transfer interval");
    pending.complete = true;
    const storage = this.storage;
    const pageCount = pending.page;
    const intervalAfter = pending.after;
    const isCurrent = () => generation === this.generation;
    return {
      ...frame,
      after: intervalAfter,
      async *pages() {
        let after = intervalAfter;
        for (let index = 0; index < pageCount; index++) {
          if (!isCurrent()) throw new Error("Entity transfer cancelled");
          const encoded = await storage.get(`${prefix}page:${index}`);
          if (encoded === undefined)
            throw new Error("Missing staged entity page");
          const batch = pageValue(JSON.parse(encoded), after, frame.cursor.seq);
          after = batch.through;
          yield batch;
        }
      },
      has: async (id) => (await storage.get(`${prefix}id:${id}`)) !== undefined,
      dispose: async () => {
        await storage.remove(prefix);
        if (this.pending.get(scope) === pending) this.pending.delete(scope);
      },
    };
  }
}
