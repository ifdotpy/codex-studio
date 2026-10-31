import { SubscriptionReplyKind } from "../generated/api";
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

/** Staging in page memory, for a page whose projections live in the native core.
 * An interval is applied only after it is complete, so a lost page loses nothing durable. */
export class MemoryTransferStorage implements TransferStorage {
  private readonly pages = new Map<string, string>();
  put(key: string, value: string) {
    this.pages.set(key, value);
    return Promise.resolve();
  }
  putMany(values: [string, string][]) {
    for (const [key, value] of values) this.pages.set(key, value);
    return Promise.resolve();
  }
  get(key: string) {
    return Promise.resolve(this.pages.get(key));
  }
  remove(prefix: string) {
    for (const key of this.pages.keys())
      if (key.startsWith(prefix)) this.pages.delete(key);
    return Promise.resolve();
  }
  close() {
    this.pages.clear();
    return Promise.resolve();
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
  declare private readonly storage: TransferStorage;
  constructor(storage: TransferStorage = new MemoryTransferStorage()) {
    this.storage = storage;
  }
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
      // Completed intervals belong to their FIFO operation until disposal.
      if (!pending.complete)
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
      // Completed intervals belong to their FIFO operation until disposal.
      if (!pending.complete)
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
      if (
        frame.replyKind === SubscriptionReplyKind.snapshot &&
        batch.after !== 0
      )
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
