import { randomUUID } from "../uuid";
import { storageKeySnapshot } from "../storageKeys";
import { maxPreferenceKeyBytes } from "./preferenceLimits";
import { DRAFT_JOURNAL_PREFIX } from "./legacyDrafts";
type Invoke = <T>(method: string, params: unknown, id?: string) => Promise<T>;
type Write = {
  id: string;
  method:
    | "preferences.raw_set"
    | "preferences.raw_remove"
    | "preferences.import";
  params: Record<string, unknown>;
};

// Core rows owned by this store. They are not preferences.
const JOURNAL = "studio-native-preferences-journal-v1";
const IMPORTED = "studio-native-preferences-imported-v1";
// WebView keys of older builds that are never imported.
const legacyOnly = (key: string) =>
  key === "studio-native-preferences-ready-v1" ||
  key === "studio-browser-profile-import-v1" ||
  key.startsWith(DRAFT_JOURNAL_PREFIX);

const maxJournalBytes = 4 * 1024 * 1024;

function serializeWrites(writes: Write[]) {
  const serialized = JSON.stringify(writes);
  if (
    writes.length > 10000 ||
    new TextEncoder().encode(serialized).length > maxJournalBytes
  )
    throw new Error(
      "Pending preference writes exceed the recovery journal limit.",
    );
  return serialized;
}

function parseJournal(raw: string | undefined): Write[] {
  if (raw === undefined) return [];
  const pending: unknown = JSON.parse(raw);
  if (
    !Array.isArray(pending) ||
    pending.length > 10000 ||
    pending.some(
      (write) =>
        !write ||
        typeof write.id !== "string" ||
        !write.id ||
        write.id.length > 128 ||
        ![
          "preferences.raw_set",
          "preferences.raw_remove",
          "preferences.import",
        ].includes(write.method) ||
        !write.params ||
        typeof write.params !== "object" ||
        Array.isArray(write.params),
    )
  )
    throw new Error(
      "The preference recovery journal is invalid. It has been preserved.",
    );
  return pending;
}

function scopedKey(scope: string, value: unknown) {
  const prefix = `browser:${JSON.stringify(scope)}:`;
  if (
    typeof value !== "string" ||
    !value ||
    new TextEncoder().encode(prefix + value).length > maxPreferenceKeyBytes
  )
    throw new Error("Invalid preference key.");
  return prefix + value;
}

// The caller, never a server frame, selects the namespace.
export function preferenceScope(
  invoke: (method: string, params: unknown, id?: string) => Promise<unknown>,
  scope: string,
): Invoke {
  const prefix = `browser:${JSON.stringify(scope)}:`;
  return async <T>(method: string, input: unknown, id?: string): Promise<T> => {
    const params = input as Record<string, unknown> | null;
    const key = (value: unknown) => scopedKey(scope, value);
    if (method === "preferences.raw_all") {
      const all = (await invoke(method, {}, id)) as Record<string, string>;
      return Object.fromEntries(
        Object.entries(all)
          .filter(([name]) => name.startsWith(prefix))
          .map(([name, value]) => [name.slice(prefix.length), value]),
      ) as T;
    }
    if (method === "preferences.import") {
      if (
        !params ||
        params.version !== 1 ||
        !params.entries ||
        typeof params.entries !== "object" ||
        Array.isArray(params.entries)
      )
        throw new Error("Invalid preference snapshot.");
      return (await invoke(
        method,
        {
          version: 1,
          entries: Object.fromEntries(
            Object.entries(params.entries).map(([name, value]) => [
              key(name),
              value,
            ]),
          ),
        },
        id,
      )) as T;
    }
    if (method !== "preferences.raw_set" && method !== "preferences.raw_remove")
      throw new Error("This preference operation is unavailable.");
    return (await invoke(
      method,
      { key: key(params?.key), raw: params?.raw },
      id,
    )) as T;
  };
}

// TODO(attar-sync): Replace the promise queue with a synchronous durable Rust
// commit. Until then a write is visible before its journal row is committed.
// A failed journal save is reported and blocks the write from being sent.
export class NativePreferenceStorage implements Storage {
  private values = new Map<string, string>();
  private keySnapshot: readonly string[] | undefined;
  private writes: Write[] = [];
  private running: Promise<void> | undefined;
  private failed = false;
  private journal: Promise<void> = Promise.resolve();
  private journalQueued = false;

  declare private invoke: Invoke;
  declare private report: (error: unknown) => void;
  declare private scope: string;
  private constructor(
    invoke: Invoke,
    report: (error: unknown) => void,
    scope: string,
  ) {
    this.invoke = invoke;
    this.report = report;
    this.scope = scope;
  }

  // The legacy callback returns WebView entries. It runs only until the core
  // marker exists. Entries import into an empty core, and an old journal is
  // replayed once with its original IDs.
  static async open(
    invoke: Invoke,
    report: (error: unknown) => void,
    legacy: () => Record<string, string>,
    scope = "root",
  ) {
    const storage = new NativePreferenceStorage(invoke, report, scope);
    let all = await storage.replay();
    if (all[IMPORTED] === undefined) {
      const source = legacy();
      const entries = Object.keys(all).length
        ? {}
        : Object.fromEntries(
            Object.entries(source).filter(
              ([key]) => !legacyOnly(key) && key !== JOURNAL,
            ),
          );
      for (const key of Object.keys(entries)) scopedKey(scope, key);
      await invoke(
        "preferences.import",
        {
          version: 1,
          entries: {
            ...entries,
            ...(source[JOURNAL] === undefined
              ? {}
              : { [JOURNAL]: source[JOURNAL] }),
            [IMPORTED]: "1",
          },
        },
        randomUUID(),
      );
      all = await storage.replay();
    }
    if (Object.values(all).some((value) => typeof value !== "string"))
      throw new Error("The native preference snapshot is invalid.");
    storage.values = new Map(
      Object.entries(all).filter(
        ([key]) => key !== JOURNAL && key !== IMPORTED,
      ),
    );
    return storage;
  }

  // Replay the core journal, then read the rows that remain.
  private async replay() {
    const before = await this.invoke<Record<string, string>>(
      "preferences.raw_all",
      {},
    );
    this.writes = parseJournal(before[JOURNAL]);
    await this.flush();
    return this.invoke<Record<string, string>>("preferences.raw_all", {});
  }

  // Each save writes the queue as it is when the save runs. Saves run one at a time.
  private persistJournal(): Promise<void> {
    if (!this.journalQueued) {
      this.journalQueued = true;
      this.journal = this.journal
        .catch(() => {})
        .then(() => {
          this.journalQueued = false;
          const serialized = serializeWrites(this.writes);
          return serialized === "[]"
            ? this.invoke(
                "preferences.raw_remove",
                { key: JOURNAL },
                randomUUID(),
              ).then(() => {})
            : this.invoke(
                "preferences.raw_set",
                { key: JOURNAL, raw: serialized },
                randomUUID(),
              ).then(() => {});
        });
    }
    return this.journal;
  }
  private enqueue(writes: Write[]) {
    // A size failure happens before the visible value changes.
    serializeWrites([...this.writes, ...writes]);
    this.writes = [...this.writes, ...writes];
    // A journal failure is reported by the next flush, which retries it.
    this.persistJournal().catch(() => {});
  }
  flush = async (): Promise<void> => {
    // Include writes queued while the previous drain's completion callbacks run.
    do {
      await this.drain();
    } while (this.writes.length);
  };
  private drain = (): Promise<void> => {
    if (this.running) return this.running;
    this.failed = false;
    const run = async () => {
      for (let write = this.writes[0]; write; write = this.writes[0]) {
        // The journal lists the write before it is sent, so a restart can resend it.
        await this.persistJournal();
        await this.invoke(write.method, write.params, write.id);
        this.writes = this.writes.slice(1);
      }
      await this.persistJournal();
    };
    this.running = run()
      .catch((error) => {
        this.failed = true;
        this.report(error);
        throw error;
      })
      .finally(() => {
        this.running = undefined;
        if (this.writes.length && !this.failed) this.start();
      });
    return this.running;
  };
  private start() {
    if (!this.failed) void this.drain().catch(() => {});
  }
  get length() {
    return this.values.size;
  }
  [storageKeySnapshot]() {
    return (this.keySnapshot ??= Object.freeze([...this.values.keys()]));
  }
  key(index: number) {
    return this[storageKeySnapshot]()[index] ?? null;
  }
  getItem(key: string) {
    return this.values.get(String(key)) ?? null;
  }
  setItem(key: string, value: string) {
    key = String(key);
    value = String(value);
    if (legacyOnly(key) || key === JOURNAL || key === IMPORTED)
      throw new Error("Invalid preference key.");
    scopedKey(this.scope, key);
    // The current value is already durable or has an exact receipt in the journal.
    // Leave that receipt in place, including after an uncertain native reply.
    if (this.values.get(key) === value) return;
    this.enqueue([
      {
        id: randomUUID(),
        method: "preferences.raw_set",
        params: { key, raw: value },
      },
    ]);
    if (!this.values.has(key)) this.keySnapshot = undefined;
    this.values.set(key, value);
    this.start();
  }
  removeItem(key: string) {
    key = String(key);
    if (!this.values.has(key)) return;
    this.enqueue([
      {
        id: randomUUID(),
        method: "preferences.raw_remove",
        params: { key },
      },
    ]);
    this.values.delete(key);
    this.keySnapshot = undefined;
    this.start();
  }
  clear() {
    this.enqueue(
      [...this.values.keys()].map((key) => ({
        id: randomUUID(),
        method: "preferences.raw_remove",
        params: { key },
      })),
    );
    this.values.clear();
    this.keySnapshot = undefined;
    this.start();
  }
}
