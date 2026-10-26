import type { NativeJournalContext } from "../mutationJournal";
import type { openNativePreferences } from "../preferenceStore";
import contract from "../../../../../contracts/preference-storage.json";
import { readNativeRevision } from "./revision";

type StoragePort = Pick<
  Awaited<ReturnType<typeof openNativePreferences>>,
  "getItem" | "setItem" | "removeItem" | "flush" | "acceptValue" | "subscribe"
>;
type Pending = { id: string; expectedRaw: string | null; raw: string | null };

/** The UI computes a pure edit. SQLite atomically admits its exact old value. */
export function createNativeLocalDrafts(
  context: NativeJournalContext,
  storage: StoragePort,
) {
  const pending = new Map<string, Pending>();
  let writing: Promise<void> = Promise.resolve();
  let stopped = false;
  const serialize = (next: unknown) => {
    const raw = next === null ? null : JSON.stringify(next);
    if (raw === undefined) throw new Error("A JSON local edit is required.");
    return raw;
  };
  const value = <T>(raw: string | null, fallback: T): T => {
    const serialized = raw === null ? JSON.stringify(fallback) : raw;
    return serialized === undefined ? fallback : JSON.parse(serialized);
  };
  function updateLocalDraft<T>(
    key: string,
    fallback: T,
    update: (current: T) => T,
  ): Promise<T> {
    if (stopped)
      return Promise.reject(
        new Error("The native local draft owner is closed."),
      );
    const operation = writing.then(async () => {
      await storage.flush();
      let saved = pending.get(key);
      if (
        saved &&
        serialize(update(value(saved.expectedRaw, fallback))) !== saved.raw
      )
        throw new Error(
          "Resolve the pending local edit before saving different content.",
        );
      for (;;) {
        if (!saved) {
          const expectedRaw = storage.getItem(key);
          const next = update(value(expectedRaw, fallback));
          const raw = serialize(next);
          const id = await context.request<string>("system.uuid", {});
          if (typeof id !== "string" || !id || id.length > 128)
            throw new Error("The native local edit identity is invalid.");
          saved = { id, expectedRaw, raw };
          pending.set(key, saved);
        }
        const reply = await context.request<{
          applied: boolean;
          raw: string | null;
          revision: string;
        }>(
          contract.rpc.compareExchange,
          { key, expectedRaw: saved.expectedRaw, raw: saved.raw },
          saved.id,
        );
        if (
          !reply ||
          typeof reply.applied !== "boolean" ||
          (reply.raw !== null && typeof reply.raw !== "string") ||
          (reply.applied && reply.raw !== saved.raw)
        )
          throw new Error("The native local edit receipt is invalid.");
        const revision = readNativeRevision(reply.revision);
        pending.delete(key);
        storage.acceptValue(key, reply.raw, revision);
        if (reply.applied) return value(storage.getItem(key), fallback);
        // Only a definitive compare refusal permits a new command identity.
        saved = undefined;
      }
    });
    writing = operation.then(
      () => {},
      () => {},
    );
    return operation;
  }
  return {
    updateLocalDraft,
    /** This updates the UI cache. The caller must await flush before an effect. */
    writeLocalDraft(key: string, next: unknown): string | null {
      try {
        if (stopped) throw new Error("The native local draft owner is closed.");
        if (next === null) storage.removeItem(key);
        else {
          const raw = JSON.stringify(next);
          if (raw === undefined)
            throw new Error("A JSON local edit is required.");
          storage.setItem(key, raw);
        }
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },
    subscribe: storage.subscribe.bind(storage),
    stop() {
      stopped = true;
    },
    async flush() {
      await writing;
      if (pending.size)
        throw new Error(
          "A local edit has an unknown native result. Keep this view open.",
        );
      await storage.flush();
    },
  };
}
