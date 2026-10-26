import { expect, it, vi } from "vitest";
import { createNativeLocalDrafts } from "./localDrafts";
import { openNativePreferences } from "../preferenceStore";
import type { NativeJournalContext } from "../mutationJournal";

function fixture(apply: (params: unknown, id?: string) => Promise<unknown>) {
  let next = 0;
  const calls: { method: string; params: unknown; id?: string }[] = [];
  const context: NativeJournalContext = {
    async request<T>(method: string, params: unknown, id?: string): Promise<T> {
      calls.push({ method, params, id });
      let value: unknown;
      if (method === "preferences.raw_all") value = {};
      else if (method === "system.uuid") value = `request-${++next}`;
      else if (method === "preferences.raw_compare_exchange")
        value = await apply(params, id);
      else throw new Error(method);
      return value as T;
    },
  };
  return { context, calls };
}

it("recomputes a pure update only after an explicit native compare refusal", async () => {
  let attempt = 0;
  const f = fixture(async () =>
    ++attempt === 1
      ? { applied: false, raw: '["other"]', revision: "1" }
      : { applied: true, raw: '["other","new"]', revision: "2" },
  );
  const storage = await openNativePreferences(f.context, vi.fn());
  const drafts = createNativeLocalDrafts(f.context, storage);
  const update = vi.fn((items: string[]) => [...items, "new"]);
  await expect(
    drafts.updateLocalDraft("attachments", [], update),
  ).resolves.toEqual(["other", "new"]);
  const writes = f.calls.filter(
    (call) => call.method === "preferences.raw_compare_exchange",
  );
  expect(writes).toEqual([
    {
      method: "preferences.raw_compare_exchange",
      params: { key: "attachments", expectedRaw: null, raw: '["new"]' },
      id: "request-1",
    },
    {
      method: "preferences.raw_compare_exchange",
      params: {
        key: "attachments",
        expectedRaw: '["other"]',
        raw: '["other","new"]',
      },
      id: "request-2",
    },
  ]);
  expect(update).toHaveBeenCalledTimes(2);
  expect(storage.getItem("attachments")).toBe('["other","new"]');
});

it("keeps the original local command after an unknown compare response", async () => {
  let lost = true;
  const f = fixture(async () => {
    if (lost) {
      lost = false;
      throw new Error("reply lost");
    }
    return { applied: true, raw: '["new"]', revision: "1" };
  });
  const storage = await openNativePreferences(f.context, vi.fn());
  const drafts = createNativeLocalDrafts(f.context, storage);
  const update = (items: string[]) => [...items, "new"];
  await expect(
    drafts.updateLocalDraft("attachments", [], update),
  ).rejects.toThrow("reply lost");
  await expect(
    drafts.updateLocalDraft("attachments", [], update),
  ).resolves.toEqual(["new"]);
  const writes = f.calls.filter(
    (call) => call.method === "preferences.raw_compare_exchange",
  );
  expect(writes).toHaveLength(2);
  expect(writes[1]).toEqual(writes[0]);
  expect(f.calls.filter((call) => call.method === "system.uuid")).toHaveLength(
    1,
  );
});

it("retries the exact uncertain removal without treating null as different content", async () => {
  let lost = true;
  const f = fixture(async () => {
    if (lost) {
      lost = false;
      throw new Error("reply lost");
    }
    return { applied: true, raw: null, revision: "1" };
  });
  const storage = await openNativePreferences(f.context, vi.fn());
  const drafts = createNativeLocalDrafts(f.context, storage);
  await expect(
    drafts.updateLocalDraft("queue", null, () => null),
  ).rejects.toThrow("reply lost");
  await expect(
    drafts.updateLocalDraft("queue", null, () => null),
  ).resolves.toBe(null);
  const writes = f.calls.filter(
    (call) => call.method === "preferences.raw_compare_exchange",
  );
  expect(writes[1]).toEqual(writes[0]);
});

it("does not replace an unknown local command with a different edit", async () => {
  const f = fixture(async () => {
    throw new Error("reply lost");
  });
  const storage = await openNativePreferences(f.context, vi.fn());
  const drafts = createNativeLocalDrafts(f.context, storage);
  await expect(
    drafts.updateLocalDraft("queue", null, () => ({ id: "original" })),
  ).rejects.toThrow("reply lost");
  await expect(
    drafts.updateLocalDraft("queue", null, () => ({ id: "different" })),
  ).rejects.toThrow(/pending|unknown/i);
  expect(
    f.calls.filter(
      (call) => call.method === "preferences.raw_compare_exchange",
    ),
  ).toHaveLength(1);
});
