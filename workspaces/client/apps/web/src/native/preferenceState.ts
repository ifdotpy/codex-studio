import { randomUUID } from "../uuid";
import { storageKeySnapshot } from "../storageKeys";
import { maxPreferenceKeyBytes } from "./preferenceLimits";
import { DRAFT_JOURNAL_PREFIX } from "./draftJournal";
type Invoke = <T>(method: string, params: unknown, id?: string) => Promise<T>;
type Write = {
  id: string;
  method:
    | "preferences.raw_set"
    | "preferences.raw_remove"
    | "preferences.import";
  params: Record<string, unknown>;
};
const JOURNAL = "studio-native-preferences-journal-v1";
const READY = "studio-native-preferences-ready-v1";
const reserved = (key: string) =>
  key === JOURNAL ||
  key === READY ||
  key === "studio-browser-profile-import-v1" ||
  key.startsWith(DRAFT_JOURNAL_PREFIX);

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

// TODO(attar-sync): Replace the WebView journal and promise queue with a
// synchronous durable Rust commit. Never acknowledge an enqueue as a commit.
export class NativePreferenceStorage implements Storage {
  private values = new Map<string, string>();
  private keySnapshot?: readonly string[];
  private writes: Write[] = [];
  private running?: Promise<void>;
  private failed = false;

  private constructor(
    private journal: Storage,
    private invoke: Invoke,
    private report: (error: unknown) => void,
    private scope: string,
  ) {}

  static async open(
    journal: Storage,
    invoke: Invoke,
    report: (error: unknown) => void,
    scope = "root",
  ) {
    const storage = new NativePreferenceStorage(journal, invoke, report, scope);
    const pending: unknown = JSON.parse(journal.getItem(JOURNAL) || "[]");
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
    storage.writes = pending;
    await storage.flush();
    let all = await invoke<Record<string, string>>("preferences.raw_all", {});
    if (journal.getItem(READY) !== "1" && !Object.keys(all).length) {
      const entries = Object.fromEntries(
        Array.from({ length: journal.length }, (_, i) => journal.key(i)!)
          .filter((key) => !reserved(key))
          .map((key) => [key, journal.getItem(key)!]),
      );
      for (const key of Object.keys(entries)) scopedKey(scope, key);
      if (Object.keys(entries).length) {
        storage.enqueue([
          {
            id: randomUUID(),
            method: "preferences.import",
            params: { version: 1, entries },
          },
        ]);
        await storage.flush();
        all = await invoke<Record<string, string>>("preferences.raw_all", {});
      }
    }
    if (Object.values(all).some((value) => typeof value !== "string"))
      throw new Error("The native preference snapshot is invalid.");
    journal.setItem(READY, "1");
    storage.values = new Map(Object.entries(all));
    return storage;
  }

  private save(writes: Write[]) {
    const serialized = JSON.stringify(writes);
    if (
      writes.length > 10000 ||
      new TextEncoder().encode(serialized).length > 4 * 1024 * 1024
    )
      throw new Error(
        "Pending preference writes exceed the recovery journal limit.",
      );
    this.journal.setItem(JOURNAL, serialized);
  }
  private enqueue(writes: Write[]) {
    const next = [...this.writes, ...writes];
    // A quota or disk failure must occur before the visible value changes.
    this.save(next);
    this.writes = next;
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
      while (this.writes.length) {
        const write = this.writes[0];
        await this.invoke(write.method, write.params, write.id);
        const next = this.writes.slice(1);
        this.save(next);
        this.writes = next;
      }
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
    if (reserved(key)) throw new Error("Invalid preference key.");
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
