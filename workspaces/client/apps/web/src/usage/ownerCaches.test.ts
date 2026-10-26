import { afterEach, expect, it, vi } from "vitest";
vi.mock("../api", () => ({
  get: vi.fn(),
  NetworkTimeoutError: class extends Error {},
}));
vi.mock("../sync/resume", () => ({ onResume: () => () => {} }));
import { createSessionCostCache } from "./sessionCostCache";
import { createProgressCache } from "../components/agents/progressCache";

function storage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  };
}
afterEach(() => vi.restoreAllMocks());
it("isolates in-memory and persisted estimates for matching session IDs", () => {
  const storageA = storage(),
    storageB = storage();
  const a = createSessionCostCache(storageA),
    b = createSessionCostCache(storageB);
  const cost = (totalUSD: number) => ({
    rootId: "same",
    pricingState: "ready",
    estimated: true,
    unknownModels: [],
    breakdown: {},
    totalUSD,
  });
  a.storeSessionCost("state", "same", cost(1));
  expect(b.peekSessionCost("state", "same")).toBeNull();
  b.storeSessionCost("state", "same", cost(9));
  expect(a.peekSessionCost("state", "same")?.totalUSD).toBe(1);
  expect(
    createSessionCostCache(storageB).peekSessionCost("state", "same")?.totalUSD,
  ).toBe(9);
});
it("keeps pending progress reads separate and discards a late response after its owner closes", async () => {
  let finishA!: (value: unknown) => void;
  const readA = vi.fn().mockImplementation(
    () =>
      new Promise((resolve) => {
        finishA = resolve;
      }),
  );
  const readB = vi.fn().mockResolvedValue({
    agent: "same",
    format: "markdown",
    markdown: "server-b",
    path: "/b/PROGRESS.md",
    revision: "b",
  });
  const a = createProgressCache(readA, storage()),
    b = createProgressCache(readB, storage());
  try {
    const pending = a.readProgress("state", "same");
    const rejected = expect(pending).rejects.toThrow("Aborted");
    expect((await b.readProgress("state", "same")).markdown).toBe("server-b");
    a.close();
    finishA({
      agent: "same",
      format: "markdown",
      markdown: "late-a",
      path: "/a/PROGRESS.md",
      revision: "a",
    });
    await rejected;
    expect(a.peekProgress("state", "same")).toBeNull();
    expect(b.peekProgress("state", "same")?.markdown).toBe("server-b");
    await expect(a.readProgress("state", "same")).rejects.toThrow("Aborted");
    expect(readA).toHaveBeenCalledOnce();
    expect(readB).toHaveBeenCalledOnce();
  } finally {
    a.close();
    b.close();
  }
});
