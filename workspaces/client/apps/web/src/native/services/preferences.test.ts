import { expect, it, vi } from "vitest";
import { openNativePreferences } from "../preferenceStore";
import type { NativeJournalContext } from "../mutationJournal";

function context(
  request: (method: string, params: unknown, id?: string) => Promise<unknown>,
): NativeJournalContext {
  return {
    request: async <T>(method: string, params: unknown, id?: string) =>
      (await request(method, params, id)) as T,
  };
}

it("binds a real preference cache to its admitted native context", async () => {
  const first = context(async () => ({}));
  const second = context(async () => ({}));
  const storage = await openNativePreferences(first, vi.fn());
  expect(() => storage.assertOwner(first)).not.toThrow();
  expect(() => storage.assertOwner(second)).toThrow(/owner/i);
});

it("publishes owner-bound preference changes without a browser event", async () => {
  const request = vi.fn(
    async (method: string, _params: unknown, _id?: string) => {
      if (method === "preferences.raw_all") return { draft: "old" };
      if (method === "system.uuid") return "native-write";
      if (method === "preferences.raw_set")
        return { saved: true, revision: "1" };
      throw new Error(method);
    },
  );
  const storage = await openNativePreferences(context(request), vi.fn());
  const changed = vi.fn();
  const stop = storage.subscribe(changed);
  storage.setItem("draft", "new");
  expect(storage.getItem("draft")).toBe("new");
  await storage.flush();
  expect(changed).toHaveBeenCalledWith("draft");
  stop();
  changed.mockClear();
  storage.acceptSnapshot({ draft: "from-host" }, "2");
  expect(storage.getItem("draft")).toBe("from-host");
  expect(changed).not.toHaveBeenCalled();
});

it("keeps optimistic UI values over an older host snapshot until exact commit completes", async () => {
  let commit!: () => void;
  const pending = new Promise<{ saved: true; revision: string }>((resolve) => {
    commit = () => resolve({ saved: true, revision: "2" });
  });
  const request = vi.fn(
    async (method: string, _params: unknown, _id?: string) => {
      if (method === "preferences.raw_all")
        return { draft: "old", other: "one" };
      if (method === "system.uuid") return "exact-write";
      if (method === "preferences.raw_set") return pending;
      throw new Error(method);
    },
  );
  const storage = await openNativePreferences(context(request), vi.fn());
  storage.setItem("draft", "typed");
  const flushed = storage.flush();
  storage.acceptSnapshot({ draft: "old", other: "two" }, "1");
  expect(storage.getItem("draft")).toBe("typed");
  expect(storage.getItem("other")).toBe("two");
  expect(storage.pending).toBe(true);
  commit();
  await flushed;
  expect(storage.pending).toBe(false);
});

it("exposes an uncertain commit and retries only its original native identity", async () => {
  let lost = true;
  const request = vi.fn(
    async (method: string, _params: unknown, _id?: string) => {
      if (method === "preferences.raw_all") return {};
      if (method === "system.uuid") return "original-write";
      if (method === "preferences.raw_set") {
        if (lost) {
          lost = false;
          throw new Error("reply lost");
        }
        return { saved: true, revision: "1" };
      }
      throw new Error(method);
    },
  );
  const report = vi.fn();
  const storage = await openNativePreferences(context(request), report);
  storage.setItem("pending", "value");
  await expect(storage.flush()).rejects.toThrow("reply lost");
  expect(storage.error).toContain("reply lost");
  expect(storage.pending).toBe(true);
  await storage.flush();
  expect(storage.error).toBe("");
  expect(
    request.mock.calls.filter(([method]) => method === "system.uuid"),
  ).toHaveLength(1);
  expect(
    request.mock.calls
      .filter(([method]) => method === "preferences.raw_set")
      .map((call) => call[2]),
  ).toEqual(["original-write", "original-write"]);
});

it("keeps a newer host value when an older raw write acknowledgement arrives later", async () => {
  let commit!: () => void;
  const pending = new Promise<unknown>((resolve) => {
    commit = () => resolve({ saved: true, revision: "8" });
  });
  const request = vi.fn(
    async (method: string, _params: unknown, _id?: string) => {
      if (method === "preferences.raw_all") return { draft: "old" };
      if (method === "system.uuid") return "exact-write";
      if (method === "preferences.raw_set") return pending;
      throw new Error(method);
    },
  );
  const storage = await openNativePreferences(context(request), vi.fn());
  storage.setItem("draft", "local");
  const flushed = storage.flush();
  storage.acceptSnapshot({ draft: "newer-peer" }, "9");
  commit();
  await flushed;
  expect(storage.getItem("draft")).toBe("newer-peer");
  storage.acceptSnapshot({ draft: "late-old" }, "8");
  expect(storage.getItem("draft")).toBe("newer-peer");
});
