import type { UsageRequest } from "./usageRequest";

const costLimit = 16;

/** In-memory stand-in for the native core usage.* commands, with the core bounds. */
export function fakeUsageCore(options: { available?: boolean } = {}) {
  const limits = new Map<string, Record<string, unknown>>();
  let costs: { scope: string; storedAt: number; value: unknown }[] = [];
  const methods: string[] = [];
  const handle = (method: string, params: unknown): unknown => {
    methods.push(method);
    if (options.available === false) throw new Error("core unavailable");
    const input = params as {
      scope: string;
      snapshot?: { accountKey: string };
      storedAt?: number;
      value?: unknown;
    };
    if (method === "usage.limits.list") return { ...limits.get(input.scope) };
    if (method === "usage.limits.put") {
      const accounts = limits.get(input.scope) ?? {};
      limits.set(input.scope, {
        ...accounts,
        [input.snapshot!.accountKey]: input.snapshot,
      });
      return null;
    }
    if (method === "usage.sessionCosts.list") return costs;
    if (method === "usage.sessionCosts.put") {
      costs = [
        ...costs.filter((row) => row.scope !== input.scope),
        { scope: input.scope, storedAt: input.storedAt!, value: input.value },
      ].sort((a, b) => a.storedAt - b.storedAt);
      const evicted = costs
        .splice(0, Math.max(0, costs.length - costLimit))
        .map((row) => row.scope);
      return { evicted };
    }
    throw new Error(`unexpected ${method}`);
  };
  const request: UsageRequest = <T>(method: string, params: unknown) =>
    new Promise<T>((resolve, reject) => {
      try {
        resolve(handle(method, structuredClone(params)) as T);
      } catch (error) {
        reject(error);
      }
    });
  return { request, methods, stored: () => ({ limits, costs }) };
}
