import { nativeRequest } from "./client";

type Batch = { ids: string[]; refillAt: number };

/** Native code creates identities. The UI can consume an unused identity synchronously. */
export function createNativeIdentityPool(load: () => Promise<Batch>, report: (error: unknown) => void) {
  const available: string[] = [];
  let refillAt = 0;
  let loading: Promise<void> | undefined;
  function refill() {
    if (loading) return loading;
    loading = load().then((batch) => {
      if (!batch || !Array.isArray(batch.ids) || !batch.ids.length || !Number.isInteger(batch.refillAt) || batch.refillAt < 0 || batch.refillAt >= batch.ids.length)
        throw new Error("The native identity batch is invalid.");
      const unique = new Set(available);
      for (const id of batch.ids) {
        if (typeof id !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id))
          throw new Error("The native identity is invalid.");
        if (unique.has(id)) throw new Error("The native identity batch contains a duplicate.");
        unique.add(id);
      }
      available.push(...batch.ids);
      refillAt = batch.refillAt;
    }).finally(() => { loading = undefined; });
    return loading;
  }
  return {
    initialize: () => loading ?? (available.length ? Promise.resolve() : refill()),
    next() {
      const id = available.shift();
      if (available.length <= refillAt) void refill().catch(report);
      if (!id) throw new Error("Wait for native request identities before this action.");
      return id;
    },
  };
}

const pool = createNativeIdentityPool(
  () => nativeRequest<Batch>("system.uuidBatch", {}),
  (error) => console.error("Native request identities are unavailable.", error),
);
export const initializeNativeIdentities = pool.initialize;
export const randomUUID = pool.next;
