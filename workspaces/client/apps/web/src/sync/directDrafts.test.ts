import { expect, it, vi } from "vitest";
import { createDirectDrafts } from "./directDrafts.ts";

const row = (text: string, seq: number) => ({
  id: "phone:chat",
  payload: JSON.stringify({
    id: "phone:chat",
    session: "chat",
    device: "phone",
    text,
    updated: seq,
  }),
  seq,
  _deleted: false,
});

it("applies a stream baseline and posts a local edit against the observed master", async () => {
  const post = vi.fn(async () => []);
  const changed = vi.fn();
  const store = createDirectDrafts({ post, changed });
  store.snapshot([row("saved", 1)], true, 1);
  expect(
    store.records().map((record) => JSON.parse(record.payload).text),
  ).toEqual(["saved"]);
  store.edit(row("edited", 2));
  await store.flush();
  expect(post).toHaveBeenCalledWith([
    {
      newDocumentState: {
        id: "phone:chat",
        payload: row("edited", 2).payload,
        seq: 2,
        deleted: false,
      },
      assumedMasterState: {
        id: "phone:chat",
        payload: row("saved", 1).payload,
        seq: 1,
        deleted: false,
      },
    },
  ]);
  expect(changed).toHaveBeenCalled();
});

it("retains a failed edit through a new stream baseline and retries it", async () => {
  const post = vi
    .fn()
    .mockRejectedValueOnce(new Error("offline"))
    .mockResolvedValue([]);
  const store = createDirectDrafts({ post, changed: vi.fn() });
  store.snapshot([row("saved", 1)], true, 1);
  store.edit(row("pending", 2));
  await expect(store.flush()).rejects.toThrow("offline");
  store.snapshot([row("saved", 1)], true, 1);
  expect(JSON.parse(store.records()[0]!.payload).text).toBe("pending");
  await store.flush();
  expect(post).toHaveBeenCalledTimes(2);
});

it("preserves a newer edit when the previous POST is still in flight", async () => {
  let finish: ((rows: ReturnType<typeof row>[]) => void) | undefined;
  const post = vi
    .fn()
    .mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    )
    .mockResolvedValue([]);
  const store = createDirectDrafts({ post, changed: vi.fn() });
  store.snapshot([row("saved", 1)], true, 1);
  store.edit(row("first", 2));
  const sending = store.flush();
  store.edit(row("newer", 3));
  finish?.([]);
  await sending;
  expect(post).toHaveBeenCalledTimes(2);
  expect(JSON.parse(store.records()[0]!.payload).text).toBe("newer");
});

it("keeps both texts when the server returns a concurrent master", async () => {
  const post = vi
    .fn()
    .mockResolvedValueOnce([row("other", 3)])
    .mockResolvedValue([]);
  const store = createDirectDrafts({ post, changed: vi.fn() });
  store.snapshot([row("saved", 1)], true, 1);
  store.edit(row("mine", 2));
  await store.flush();
  const version = JSON.parse(store.records()[0]!.payload);
  expect(version.text).toBe("other");
  expect(version.alternatives).toContain("mine");
  expect(post).toHaveBeenCalledTimes(2);
});
