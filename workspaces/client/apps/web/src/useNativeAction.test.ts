import { afterEach, expect, it, vi } from "vitest";
import type { Agent } from "./types";
const selected = vi.hoisted(() => ({
  storage: undefined as Storage | undefined,
  post: vi.fn(),
}));
vi.mock("./servers/ServerServicesContext", () => ({
  useServerStorage: () => selected.storage,
  useServerApi: () => ({ post: selected.post }),
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (value: unknown) => [value, () => {}],
}));
import { useNativeAction } from "./useNativeAction";
function memoryStorage(): Storage {
  const data = new Map<string, string>();
  return {
    get length() {
      return data.size;
    },
    key: (index) => [...data.keys()][index] ?? null,
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => {
      data.set(key, value);
    },
    removeItem: (key) => {
      data.delete(key);
    },
    clear: () => data.clear(),
  };
}
afterEach(() => vi.clearAllMocks());
it("retries an uncertain action with its original request and server after another owner is selected", async () => {
  const postA = vi.fn().mockRejectedValue(new Error("Unknown outcome"));
  const postB = vi.fn().mockRejectedValue(new Error("Unknown outcome"));
  selected.storage = memoryStorage();
  selected.post = postA;
  const a = useNativeAction("/same/path", "same-workspace", () => {});
  selected.storage = memoryStorage();
  selected.post = postB;
  const b = useNativeAction("/same/path", "same-workspace", () => {});
  const agent = {
    id: "same-chat",
    accountKey: "default",
    threadId: "same-thread",
    epoch: 0,
  } as Agent;
  await expect(a.submit(agent, "compact")).rejects.toThrow("Unknown outcome");
  await expect(b.submit(agent, "review")).rejects.toThrow("Unknown outcome");
  const requestA = a.pending(agent.id)!;
  const requestB = b.pending(agent.id)!;
  expect(requestA.action).toBe("compact");
  expect(requestB.action).toBe("review");
  expect(requestA.request_id).not.toBe(requestB.request_id);
  await expect(a.submit(agent, "compact")).rejects.toThrow("Unknown outcome");
  expect(postA.mock.calls[1]?.[1]).toEqual(requestA);
  expect(postB).toHaveBeenCalledTimes(1);
  expect(b.pending(agent.id)).toEqual(requestB);
});
