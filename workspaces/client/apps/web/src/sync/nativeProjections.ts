import type { RxCollection } from "rxdb";
import { nativeRequest } from "../native/client";
import type { SyncDocument } from "./client";
import { NATIVE_PROJECTION_PREFIX } from "./entityProjectionWrites";

/**
 * The native core stores the page's projection cache (entity rows, cursors,
 * baselines, transcript rows) in its SQLite store (`projections.rs`, rows
 * `projection.*Rows`). This module gives the sync client the narrow slice of
 * the RxDB collection API it uses, backed by that store. It never touches
 * IndexedDB, Web Locks or BroadcastChannel; the page is the only writer, so
 * one in-process queue orders the writes.
 */

type Row = SyncDocument & Record<string, unknown>;
type Request = <T>(method: string, params: unknown) => Promise<T>;
type Range = { gte?: string; gt?: string; lt: string };
/** The change event RxDB emits for each written document. */
type Change = { id: string; documentData: Row };
type Listener = (change: Change) => void;

/** Rows per native request. Owner: `MAX_ROW_BATCH` in `projections.rs`, which refuses more. */
export const ROW_BATCH = 500;
/** Upper bound of a request, in bytes of row JSON; one native message holds it. */
const REQUEST_BYTES = 4 * 1024 * 1024;
/** Sorts after every row id; ends a range that has no upper bound. */
const AFTER_ALL_IDS = "￿";
const RX_FIELDS = ["_attachments", "_meta", "_rev"] as const;

function plain(row: Row): Row {
  const copy = { ...row };
  for (const field of RX_FIELDS) delete copy[field];
  copy._deleted = copy._deleted === true;
  return copy;
}

/** RxDB documents expose their data and `toJSON()`; the rows here do the same. */
function document(row: Row): Row {
  return Object.defineProperty({ ...row }, "toJSON", {
    value: () => ({ ...row }),
  }) as Row;
}

function stable(value: unknown): string {
  return JSON.stringify(value, (_key, entry: unknown) =>
    entry && typeof entry === "object" && !Array.isArray(entry)
      ? Object.fromEntries(
          Object.entries(entry as Record<string, unknown>).sort(([a], [b]) =>
            a < b ? -1 : a > b ? 1 : 0,
          ),
        )
      : entry,
  );
}

function rangesOf(selector: Record<string, unknown>): Range[] {
  const one = (value: unknown): Range => {
    const id = (value as { id?: unknown } | undefined)?.id;
    if (typeof id === "string") return { gte: id, lt: `${id}\u0000` };
    const bounds = id as Record<string, unknown> | undefined;
    if (!bounds || typeof bounds !== "object")
      throw new Error("Unsupported projection query.");
    const lower = bounds.$gt === undefined ? bounds.$gte : bounds.$gt;
    if (typeof lower !== "string" && lower !== undefined)
      throw new Error("Unsupported projection query.");
    const upper = bounds.$lt === undefined ? AFTER_ALL_IDS : bounds.$lt;
    if (typeof upper !== "string")
      throw new Error("Unsupported projection query.");
    return {
      ...(bounds.$gt === undefined
        ? { gte: (lower as string | undefined) ?? "" }
        : { gt: lower as string }),
      lt: upper,
    };
  };
  const alternatives = selector.$or;
  return Array.isArray(alternatives) ? alternatives.map(one) : [one(selector)];
}

class NativeProjectionCollection {
  readonly database: { name: string };
  readonly storageInstance: {
    findDocumentsById: (ids: string[], withDeleted: boolean) => Promise<Row[]>;
    bulkWrite: (
      writes: Array<{ previous?: Row; document: Row }>,
      context: string,
    ) => Promise<{ success: Row[]; error: Array<{ status: number }> }>;
  };
  readonly $: {
    subscribe: (
      observer: Listener | { next: Listener; error?: (error: unknown) => void },
    ) => { unsubscribe(): void };
  };
  private readonly listeners = new Set<Listener>();
  private writes: Promise<unknown> = Promise.resolve();
  declare private readonly workspace: string;
  declare private readonly prefix: string;
  declare private readonly request: Request;

  constructor(
    workspace: string,
    prefix: string,
    name: string,
    request: Request,
  ) {
    this.workspace = workspace;
    this.prefix = prefix;
    this.request = request;
    this.database = { name };
    this.storageInstance = {
      findDocumentsById: (ids, withDeleted) => this.read(ids, withDeleted),
      bulkWrite: (writes) => this.write(writes),
    };
    this.$ = {
      subscribe: (observer) => {
        const next = typeof observer === "function" ? observer : observer.next;
        this.listeners.add(next);
        return { unsubscribe: () => this.listeners.delete(next) };
      },
    };
  }

  private call<T>(method: string, params: Record<string, unknown>) {
    return this.request<T>(method, {
      workspace: this.workspace,
      prefix: this.prefix,
      ...params,
    });
  }

  private async read(ids: string[], withDeleted: boolean) {
    const found = new Map<string, Row>();
    for (let start = 0; start < ids.length; start += ROW_BATCH) {
      const rows = await this.call<Row[]>("projection.getRows", {
        ids: ids.slice(start, start + ROW_BATCH),
      });
      for (const row of rows) found.set(row.id, plain(row));
    }
    return ids.flatMap((id) => {
      const row = found.get(id);
      return row && (withDeleted || !row._deleted) ? [row] : [];
    });
  }

  private async range(range: Range, limit: number) {
    const rows: Row[] = [];
    let after: string | undefined;
    for (;;) {
      const bound = after === undefined ? range : { gt: after, lt: range.lt };
      const reply = await this.call<{ rows: Row[]; more: boolean }>(
        "projection.rangeRows",
        { ...bound, limit: limit - rows.length },
      );
      for (const row of reply.rows) rows.push(plain(row));
      if (!reply.more || rows.length >= limit || !reply.rows.length) break;
      after = reply.rows.at(-1)!.id;
    }
    return rows;
  }

  /** The live rows of a query, ordered by id. */
  async query(selector: Record<string, unknown>, limit = Infinity) {
    const rows = new Map<string, Row>();
    for (const range of rangesOf(selector))
      for (const row of await this.range(range, limit))
        if (!row._deleted) rows.set(row.id, row);
    return [...rows.values()]
      .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
      .slice(0, limit);
  }

  private write(writes: Array<{ previous?: Row; document: Row }>) {
    // Writers hold this queue, so the optimistic check below sees every
    // earlier write. A caller whose base row changed gets a conflict, as in RxDB.
    const run = async () => {
      const ids = writes.map(({ document: row }) => row.id);
      const current = new Map(
        (await this.read(ids, true)).map((row) => [row.id, row]),
      );
      const success: Row[] = [];
      const error: Array<{ status: number }> = [];
      const accepted: Row[] = [];
      for (const { previous, document: row } of writes) {
        const stored = current.get(row.id);
        const same =
          stored && previous && stable(stored) === stable(plain(previous));
        if (previous ? !same : stored) error.push({ status: 409 });
        else accepted.push(plain(row));
      }
      let batch: Row[] = [];
      let bytes = 0;
      const flush = async () => {
        if (batch.length)
          await this.call("projection.putRows", { rows: batch });
        batch = [];
        bytes = 0;
      };
      for (const row of accepted) {
        const size = JSON.stringify(row).length;
        if (
          batch.length >= ROW_BATCH ||
          (batch.length && bytes + size > REQUEST_BYTES)
        )
          await flush();
        batch.push(row);
        bytes += size;
      }
      await flush();
      for (const row of accepted) {
        success.push(row);
        for (const listener of this.listeners)
          listener({ id: row.id, documentData: row });
      }
      return { success, error };
    };
    const result = this.writes.then(run, run);
    this.writes = result.catch(() => {});
    return result;
  }

  find(query: { selector: Record<string, unknown>; limit?: number }) {
    const ranges = rangesOf(query.selector);
    const covers = (id: string) =>
      ranges.some(
        (range) =>
          (range.gt === undefined ? id >= range.gte! : id > range.gt) &&
          id < range.lt,
      );
    return {
      exec: async () =>
        (await this.query(query.selector, query.limit)).map(document),
      $: this.observe(covers, async () =>
        (await this.query(query.selector, query.limit)).map(document),
      ),
    };
  }

  findOne(id: string) {
    const load = async () => {
      const [row] = await this.read([id], false);
      return row ? document(row) : null;
    };
    return {
      exec: load,
      $: this.observe((changed) => changed === id, load),
    };
  }

  /** Emit the result now and after each write that touches the query. */
  private observe<T>(covers: (id: string) => boolean, load: () => Promise<T>) {
    return {
      subscribe: (
        observer:
          | ((value: T) => void)
          | {
              next: (value: T) => void;
              error?: (error: unknown) => void;
            },
      ) => {
        const next = typeof observer === "function" ? observer : observer.next;
        const fail =
          typeof observer === "function" ? undefined : observer.error;
        let stopped = false;
        let running = false;
        let again = false;
        const emit = async () => {
          if (running) {
            again = true;
            return;
          }
          running = true;
          try {
            do {
              again = false;
              const value = await load();
              if (!stopped) next(value);
            } while (again && !stopped);
          } catch (error) {
            if (!stopped) fail?.(error);
          } finally {
            running = false;
          }
        };
        const listener: Listener = (change) => {
          if (covers(change.id)) void emit();
        };
        this.listeners.add(listener);
        void emit();
        return {
          unsubscribe: () => {
            stopped = true;
            this.listeners.delete(listener);
          },
        };
      },
    };
  }
}

/** One cache per workspace and API schema; sync clients of the same server share it. */
export function createNativeProjectionStore(request: Request = nativeRequest) {
  const open = new Map<string, NativeProjectionCollection>();
  return {
    /** Open the cache of one workspace and schema, and drop caches of older schemas. */
    async open(workspaceId: string, schemaHash: string, serverSuffix: string) {
      const prefix = `${schemaHash}${serverSuffix}`.replace(
        /[^A-Za-z0-9_.-]/g,
        "-",
      );
      const key = `${workspaceId}/${prefix}`;
      let collection = open.get(key);
      if (!collection) {
        collection = new NativeProjectionCollection(
          workspaceId,
          prefix,
          `${NATIVE_PROJECTION_PREFIX}${key}`,
          request,
        );
        open.set(key, collection);
      }
      // The class implements the RxCollection members the sync client uses.
      const shim: object = collection;
      return shim as RxCollection<SyncDocument>;
    },
    async pruneOtherSchemas(
      workspaceId: string,
      schemaHash: string,
      serverSuffix: string,
    ) {
      const prefix = `${schemaHash}${serverSuffix}`.replace(
        /[^A-Za-z0-9_.-]/g,
        "-",
      );
      await request("projection.pruneRows", { workspace: workspaceId, prefix });
    },
  };
}

export const nativeProjectionStore = createNativeProjectionStore();
