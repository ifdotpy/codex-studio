import contract from "../../../../../contracts/preference-storage.json";
import type { PreferenceFields } from "../../sync/uiPreferenceMerge";
import type { NativeServerContext } from "../client";
import type { NativePreferencePort } from "./types";
import type { createNativeServiceLifecycle } from "./lifecycle";
import { compareNativeRevisions, readNativeRevision } from "./revision";
import {
  readNativePreferenceSync,
  type NativePreferenceSync,
} from "./preferenceStatus";

export type NativePreferenceSnapshot = {
  values: Record<string, string>;
  fields: { user: PreferenceFields; server: PreferenceFields };
  revision: string;
  sync: NativePreferenceSync;
};
function record(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
export function readNativePreferenceSnapshot(
  value: unknown,
): NativePreferenceSnapshot {
  if (
    !record(value) ||
    !record(value.values) ||
    Object.values(value.values).some((raw) => typeof raw !== "string") ||
    !record(value.fields) ||
    !record(value.fields.user) ||
    !record(value.fields.server)
  )
    throw new Error("The native preference view is invalid.");
  readNativeRevision(value.revision);
  readNativePreferenceSync(value.sync);
  return value as NativePreferenceSnapshot;
}

/** Rust owns preference scope, field merge, writer identity and edit time. */
export function createNativePreferenceDomain(
  context: NativeServerContext,
  storage: NativePreferencePort,
  lifecycle: ReturnType<typeof createNativeServiceLifecycle>,
  report: (error: unknown) => void,
  presentStatus: (sync: NativePreferenceSync) => void,
) {
  let snapshot: NativePreferenceSnapshot | undefined;
  let initialized = false;
  let stopped = false;
  let initialization: Promise<void> | undefined;
  let writing: Promise<void> | undefined;
  const writes: Array<{
    id?: string;
    method: string;
    params: Record<string, unknown>;
  }> = [];
  const apply = (value: unknown) => {
    const next = readNativePreferenceSnapshot(value);
    if (
      snapshot &&
      compareNativeRevisions(next.revision, snapshot.revision) < 0
    )
      return;
    if (compareNativeRevisions(next.revision, storage.revision) < 0) return;
    snapshot = next;
    storage.acceptSnapshot(next.values, next.revision);
    presentStatus(next.sync);
  };
  const flush = (): Promise<void> => {
    if (writing) return writing;
    writing = (async () => {
      await storage.flush();
      while (writes.length) {
        const write = writes[0];
        write.id ??= await context.request<string>("system.uuid", {});
        if (!write.id || typeof write.id !== "string" || write.id.length > 128)
          throw new Error("The native preference edit identity is invalid.");
        const result = await context.request<unknown>(
          write.method,
          write.params,
          write.id,
        );
        apply(result);
        writes.shift();
      }
    })()
      .catch((error) => {
        report(error);
        throw error;
      })
      .finally(() => {
        writing = undefined;
      });
    return writing;
  };
  const enqueue = (method: string, params: Record<string, unknown>) => {
    if (stopped) throw new Error("The native preference owner is closed.");
    writes.push({ method, params: JSON.parse(JSON.stringify(params)) });
    void flush().catch(() => {});
  };
  return {
    initialize() {
      if (stopped)
        return Promise.reject(
          new Error("The native preference owner is closed."),
        );
      if (!initialization)
        initialization = (async () => {
          apply(await context.request(contract.rpc.view, {}));
          if (stopped)
            throw new Error(
              "The native preference owner closed before readiness.",
            );
          if (!initialized)
            lifecycle.watch<NativePreferenceSnapshot>(
              contract.topics.preferences,
              {},
              (event) => {
                if (event.error) report(new Error(event.error.message));
                else if (event.value) {
                  try {
                    apply(event.value);
                  } catch (error) {
                    report(error);
                  }
                }
              },
            );
          initialized = true;
        })().catch((error) => {
          initialization = undefined;
          throw error;
        });
      return initialization;
    },
    editPreference<T extends Record<string, unknown>>(
      key: string,
      previous: T,
      next: T,
    ): T {
      enqueue(contract.rpc.edit, { key, previous, next });
      // The return is an immediate display proposal, not a commit receipt.
      return next;
    },
    preferenceFields() {
      if (!snapshot)
        throw new Error(
          "Wait for the native preference view before opening this screen.",
        );
      return snapshot.fields.server;
    },
    adoptPreferenceFields(_fields?: PreferenceFields) {
      // The host reads its authenticated committed projection. No UI clocks cross IPC.
      enqueue(contract.rpc.adoptFields, {});
    },
    subscribePreference(key: string, update: () => void) {
      return storage.subscribe((changed) => {
        if (changed === null || changed === key) update();
      });
    },
    flush,
    stop() {
      stopped = true;
    },
  };
}
