import type { SyncDocument } from "./client";

/**
 * An in-memory document collection. The sync client reads and writes its
 * projections and drafts through this narrow surface. Memory is the
 * only store the UI waits for. A collection may take a `durable` hook that
 * persists a row before the write is acknowledged; unsent input uses it.
 */

export type Row = SyncDocument & Record<string, unknown>;
type Range = { gte?: string; gt?: string; lt: string };
/** Sorts after every row id; ends a range that has no upper bound. */
const AFTER_ALL_IDS = "￿";

/** A row plus the helpers that callers use on a stored document. */
export type Doc = Row & {
  getLatest(): Doc;
  incrementalModify(change: (row: Row) => Row): Promise<Doc>;
  incrementalPatch(patch: Partial<Row>): Promise<Doc>;
  toJSON(): Row;
};
export type Change = { id: string; documentData: Row; context: string };
type Listener = (change: Change) => void;
type Observer<T> =
  | ((value: T) => void)
  | { next: (value: T) => void; error?: (error: unknown) => void };

export type Durable = {
  /** Persist a row before its write is acknowledged. */
  put(row: Row): Promise<void>;
};

export type CollectionOptions = {
  durable?: Durable;
  /** Reads and writes of these ids wait for the returned promise (lazy cache load). */
  ensure?: (id: string) => Promise<void> | undefined;
  /** Called with the ids of rows that changed in memory. */
  onChange?: (change: Change) => void;
};

export class WriteConflictError extends Error {
  readonly status = 409;
  constructor(id: string) {
    super(`Document ${id} was changed by another writer.`);
  }
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

function plain(row: Row): Row {
  const copy = { ...row };
  delete (copy as { getLatest?: unknown }).getLatest;
  copy._deleted = copy._deleted === true;
  return copy;
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
  if (!Object.keys(selector).length) return [{ gte: "", lt: AFTER_ALL_IDS }];
  const alternatives = selector.$or;
  return Array.isArray(alternatives) ? alternatives.map(one) : [one(selector)];
}

const inRange = (range: Range, id: string) =>
  (range.gt === undefined ? id >= (range.gte ?? "") : id > range.gt) &&
  id < range.lt;

export class LocalCollection {
  readonly database: { name: string };
  readonly storageInstance: {
    findDocumentsById: (ids: string[], withDeleted: boolean) => Promise<Row[]>;
    bulkWrite: (
      writes: Array<{ previous?: Row; document: Row }>,
      context: string,
    ) => Promise<{ success: Row[]; error: Array<{ status: number }> }>;
  };
  readonly $: {
    subscribe: (observer: Observer<Change>) => { unsubscribe(): void };
  };
  private readonly rows = new Map<string, Row>();
  private readonly docs = new WeakMap<Row, Doc>();
  private readonly listeners = new Set<Listener>();
  private writes: Promise<unknown> = Promise.resolve();
  declare private readonly options: CollectionOptions;

  constructor(name: string, options: CollectionOptions = {}) {
    this.options = options;
    this.database = { name };
    this.storageInstance = {
      findDocumentsById: (ids, withDeleted) => this.read(ids, withDeleted),
      bulkWrite: (writes, context) => this.write(writes, context),
    };
    this.$ = {
      subscribe: (observer) => {
        const next = typeof observer === "function" ? observer : observer.next;
        this.listeners.add(next);
        return { unsubscribe: () => this.listeners.delete(next) };
      },
    };
  }

  private async ensure(ids: Iterable<string>) {
    const ensure = this.options.ensure;
    if (!ensure) return;
    const waits = new Set<Promise<void>>();
    for (const id of ids) {
      const wait = ensure(id);
      if (wait) waits.add(wait);
    }
    await Promise.all(waits);
  }

  private async read(ids: string[], withDeleted: boolean) {
    await this.ensure(ids);
    return ids.flatMap((id) => {
      const row = this.rows.get(id);
      return row && (withDeleted || !row._deleted) ? [row] : [];
    });
  }

  /** The live rows of a query, ordered by id. */
  async query(selector: Record<string, unknown>, limit = Infinity) {
    const ranges = rangesOf(selector);
    await this.ensure(ranges.map((range) => range.gte ?? range.gt ?? ""));
    return [...this.rows.values()]
      .filter((row) => !row._deleted && ranges.some((r) => inRange(r, row.id)))
      .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
      .slice(0, limit);
  }

  private doc(row: Row): Doc {
    let doc = this.docs.get(row);
    if (!doc) {
      const copy = { ...row } as Doc;
      Object.defineProperties(copy, {
        getLatest: {
          value: () => {
            const latest = this.rows.get(row.id);
            return latest && latest !== row ? this.doc(latest) : doc!;
          },
        },
        incrementalModify: {
          value: (change: (row: Row) => Row) => this.modify(row.id, change),
        },
        incrementalPatch: {
          value: (patch: Partial<Row>) =>
            this.modify(row.id, (latest) => ({ ...latest, ...patch })),
        },
        toJSON: { value: () => ({ ...row }) },
      });
      doc = copy;
      this.docs.set(row, doc);
    }
    return doc;
  }

  private emit(row: Row, context: string) {
    const change = { id: row.id, documentData: row, context };
    this.options.onChange?.(change);
    for (const listener of this.listeners) listener(change);
  }

  private async write(
    writes: Array<{ previous?: Row; document: Row }>,
    context = "",
  ) {
    // Cached rows load before the queue; loading uses the queue itself.
    await this.ensure(writes.map(({ document: row }) => row.id));
    // One queue orders every writer, so the optimistic check below sees every
    // earlier write. A caller whose base row changed gets a conflict.
    const run = async () => {
      const success: Row[] = [];
      const error: Array<{ status: number }> = [];
      const accepted: Row[] = [];
      for (const { previous, document: row } of writes) {
        const stored = this.rows.get(row.id);
        const same =
          stored &&
          previous &&
          (stored === previous || stable(stored) === stable(plain(previous)));
        if (previous ? !same : stored) error.push({ status: 409 });
        else accepted.push(plain(row));
      }
      const durable = this.options.durable;
      if (durable && context === "local")
        for (const row of accepted) await durable.put(row);
      for (const row of accepted) {
        this.rows.set(row.id, row);
        success.push(row);
      }
      for (const row of accepted) this.emit(row, context);
      return { success, error };
    };
    const result = this.writes.then(run, run);
    this.writes = result.catch(() => {});
    return result;
  }

  /** Add cached rows that are absent or older than the cache. Newer rows win. */
  async hydrate(rows: Row[]) {
    const run = async () => {
      const added: Row[] = [];
      for (const row of rows) {
        const current = this.rows.get(row.id);
        if (current && current.seq >= row.seq) continue;
        const stored = plain(row);
        this.rows.set(stored.id, stored);
        added.push(stored);
      }
      for (const row of added) this.emit(row, "cache");
    };
    const result = this.writes.then(run, run);
    this.writes = result.catch(() => {});
    await result;
  }

  /** Every stored row, tombstones included. */
  all() {
    return [...this.rows.values()];
  }

  async insert(row: Row) {
    const result = await this.write([{ document: row }], "local");
    if (result.error.length) throw new WriteConflictError(row.id);
    return this.doc(result.success[0]!);
  }

  /** Write a row; keep going from the latest row if another writer got in first. */
  async incrementalUpsert(row: Row) {
    for (;;) {
      const [previous] = await this.read([row.id], true);
      const result = await this.write(
        [{ ...(previous ? { previous } : {}), document: row }],
        "local",
      );
      if (!result.error.length) return this.doc(result.success[0]!);
    }
  }

  private async modify(id: string, change: (row: Row) => Row) {
    for (;;) {
      const [previous] = await this.read([id], true);
      if (!previous) throw new Error(`Document ${id} does not exist.`);
      const result = await this.write(
        [{ previous, document: change(plain(previous)) }],
        "local",
      );
      if (!result.error.length) return this.doc(result.success[0]!);
    }
  }

  find(query: { selector?: Record<string, unknown>; limit?: number } = {}) {
    const selector = query.selector ?? {};
    const ranges = rangesOf(selector);
    const load = async () =>
      (await this.query(selector, query.limit)).map((row) => this.doc(row));
    return {
      exec: load,
      $: this.observe((id) => ranges.some((range) => inRange(range, id)), load),
    };
  }

  findOne(id: string) {
    const load = async () => {
      const [row] = await this.read([id], false);
      return row ? this.doc(row) : null;
    };
    return { exec: load, $: this.observe((changed) => changed === id, load) };
  }

  /** Emit the result now and after each write that touches the query. */
  private observe<T>(covers: (id: string) => boolean, load: () => Promise<T>) {
    return {
      subscribe: (
        observer: Observer<T>,
        onError?: (error: unknown) => void,
      ) => {
        const next = typeof observer === "function" ? observer : observer.next;
        const fail = typeof observer === "function" ? onError : observer.error;
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
        const subscription = {
          closed: false,
          unsubscribe: () => {
            stopped = true;
            subscription.closed = true;
            this.listeners.delete(listener);
          },
        };
        return subscription;
      },
    };
  }
}
