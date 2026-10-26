import type { MonitorInputEntry } from "../servers/monitorInputStore";
import journalContract from "../../../../contracts/mutation-journal.json";

export type NativeJournalContext = {
  request<T>(method: string, params: unknown, exactId?: string): Promise<T>;
};

// All state transitions run in the host-selected SQLite store. No browser
// persistence, digest computation, or read/write compare-and-swap belongs here.
export function createNativeMutationJournal(context: NativeJournalContext) {
  async function begin(
    path: string,
    body: unknown,
    explicitRequestId?: string,
  ) {
    if (path === "/api/sync/subscriptions") return null;
    const value =
      body && typeof body === "object" ? (body as Record<string, unknown>) : {};
    const explicit = [
      explicitRequestId,
      value.requestId,
      value.request_id,
      ...(["/api/messages", "/api/leads", "/api/assets"].includes(path)
        ? [value.id]
        : []),
    ].find((id): id is string => typeof id === "string" && !!id);
    const monitor = path === journalContract.monitorInputPath;
    if (monitor && (typeof value.id !== "string" || !value.id))
      throw new Error("A monitor ID is required for input.");
    const serializedPayload = JSON.stringify(body);
    if (serializedPayload === undefined)
      throw new Error("A JSON mutation payload is required.");
    const family = monitor ? "monitor_input" : "mutation";
    const scope = monitor ? { monitorId: value.id as string } : {};
    const entry = await context.request<MonitorInputEntry>(`${family}.begin`, {
      ...(monitor ? scope : { path }),
      serializedPayload,
      ...(explicit ? { explicitRequestId: explicit } : {}),
    });
    if (JSON.stringify(body) !== serializedPayload)
      throw new Error(
        "The mutation payload changed before its identity was saved.",
      );
    const key = {
      ...scope,
      requestId: entry.requestId,
      payloadDigest: entry.payloadDigest,
    };
    return {
      requestId: entry.requestId,
      finish: () => context.request(`${family}.settle`, key),
      unknown: () => context.request(`${family}.unknown`, key),
      reject(status: number, result: unknown) {
        const code =
          result && typeof result === "object"
            ? (result as Record<string, unknown>).code
            : undefined;
        return context.request(`${family}.reject`, {
          ...key,
          status,
          ...(typeof code === "string" ? { code } : {}),
        });
      },
    };
  }
  return Object.defineProperty(begin, "nativeMonitorInput", {
    value: Object.freeze({
      pending: (monitorId: string) =>
        context.request<MonitorInputEntry | null>("monitor_input.pending", {
          monitorId,
        }),
      settle: (monitorId: string, requestId: string) =>
        context.request<boolean>("monitor_input.settle", {
          monitorId,
          requestId,
        }),
    }),
  }) as typeof begin & {
    readonly nativeMonitorInput: {
      pending(monitorId: string): Promise<MonitorInputEntry | null>;
      settle(monitorId: string, requestId: string): Promise<boolean>;
    };
  };
}
