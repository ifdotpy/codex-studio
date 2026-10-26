import { storageKeySnapshot } from "../storageKeys";
import type { NativeJournalContext } from "./mutationJournal";
import {
  readNativeRevision,
  compareNativeRevisions,
} from "./services/revision";
import contract from "../../../../contracts/preference-storage.json";

type Write = { id?: string; method: string; params: Record<string, unknown> };

// This cache is UI state, not a durability acknowledgement. Only flush resolves
// after every queued command has a SQLite receipt. No WebView journal is used.
class NativePreferences implements Storage {
  private writes: Write[] = [];
  private running?: Promise<void>;
  private snapshot?: readonly string[];
  private failure = "";
  private version = "0";
  private authoritative: Map<string, string>;
  private readonly listeners = new Set<(key: string | null) => void>();
  constructor(
    private context: NativeJournalContext,
    private values: Map<string, string>,
    private report: (error: unknown) => void,
  ) {
    this.authoritative = new Map(values);
  }
  get length() {
    return this.values.size;
  }
  get pending() {
    return this.writes.length > 0;
  }
  get error() {
    return this.failure;
  }
  get revision() {
    return this.version;
  }
  assertOwner(context: NativeJournalContext) {
    if (context !== this.context)
      throw new Error("The native preference cache has another owner.");
  }
  subscribe(listener: (key: string | null) => void) {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }
  private publish(key: string | null) {
    for (const listener of this.listeners) listener(key);
  }
  /** A native event updates the UI cache, never the native command queue. */
  acceptSnapshot(values: Record<string, string>, revision: string) {
    const version = readNativeRevision(revision);
    if (
      !values ||
      Array.isArray(values) ||
      typeof values !== "object" ||
      Object.values(values).some((value) => typeof value !== "string")
    )
      throw new Error("The native preference snapshot is invalid.");
    if (compareNativeRevisions(version, this.version) < 0) return;
    this.version = version;
    this.authoritative = new Map(Object.entries(values));
    this.project();
  }
  private project() {
    const next = new Map(this.authoritative);
    // A late event must not replace an edit whose commit is still uncertain.
    for (const write of this.writes) {
      const key = String(write.params.key);
      if (write.method === "preferences.raw_remove") next.delete(key);
      else next.set(key, String(write.params.raw));
    }
    const changed = new Set([...this.values.keys(), ...next.keys()]);
    for (const key of changed)
      if (this.values.get(key) === next.get(key)) changed.delete(key);
    this.values = next;
    this.snapshot = undefined;
    for (const key of changed) this.publish(key);
  }
  acceptValue(key: string, raw: string | null, revision: string) {
    const version = readNativeRevision(revision);
    if (compareNativeRevisions(version, this.version) < 0) return;
    const values = Object.fromEntries(this.authoritative);
    if (raw === null) delete values[key];
    else values[key] = raw;
    this.acceptSnapshot(values, version);
  }
  [storageKeySnapshot]() {
    return (this.snapshot ??= Object.freeze([...this.values.keys()]));
  }
  key(index: number) {
    return this[storageKeySnapshot]()[index] ?? null;
  }
  getItem(key: string) {
    return this.values.get(String(key)) ?? null;
  }
  private validate(key: string) {
    if (!key || key.startsWith("browser:"))
      throw new Error("Invalid native preference key.");
  }
  setItem(key: string, raw: string) {
    key = String(key);
    raw = String(raw);
    this.validate(key);
    if (this.values.get(key) === raw) return;
    this.writes.push({ method: contract.rpc.rawSet, params: { key, raw } });
    this.project();
    void this.flush().catch(() => {});
  }
  removeItem(key: string) {
    key = String(key);
    this.validate(key);
    if (!this.values.has(key)) return;
    this.writes.push({ method: contract.rpc.rawRemove, params: { key } });
    this.project();
    void this.flush().catch(() => {});
  }
  clear() {
    for (const key of this.values.keys())
      this.writes.push({ method: contract.rpc.rawRemove, params: { key } });
    this.project();
    void this.flush().catch(() => {});
  }
  flush(): Promise<void> {
    if (this.running) return this.running;
    const drain = async () => {
      while (this.writes.length) {
        const write = this.writes[0];
        if (!write.id) {
          const id = await this.context.request<string>("system.uuid", {});
          if (typeof id !== "string" || !id || id.length > 128)
            throw new Error("The native preference identity is invalid.");
          write.id = id;
        }
        const receipt = await this.context.request<{
          saved: boolean;
          revision: string;
        }>(write.method, write.params, write.id);
        if (receipt?.saved !== true)
          throw new Error("The native preference commit was not confirmed.");
        const revision = readNativeRevision(receipt.revision);
        this.writes.shift();
        this.acceptValue(
          String(write.params.key),
          write.method === contract.rpc.rawRemove
            ? null
            : String(write.params.raw),
          revision,
        );
        this.project();
      }
    };
    this.running = drain()
      .catch((error) => {
        this.failure = error instanceof Error ? error.message : String(error);
        this.publish(null);
        this.report(error);
        throw error;
      })
      .finally(() => {
        this.running = undefined;
        if (!this.writes.length) this.failure = "";
        this.publish(null);
      });
    return this.running;
  }
}

export async function openNativePreferences(
  context: NativeJournalContext,
  report: (error: unknown) => void,
) {
  const values = await context.request<Record<string, string>>(
    "preferences.raw_all",
    {},
  );
  if (
    !values ||
    Array.isArray(values) ||
    typeof values !== "object" ||
    Object.values(values).some((value) => typeof value !== "string")
  )
    throw new Error("The native preference snapshot is invalid.");
  return new NativePreferences(
    context,
    new Map(Object.entries(values)),
    report,
  );
}
