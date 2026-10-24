import { createHash, webcrypto } from "node:crypto";
import { readFileSync } from "node:fs";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("./environment", () => ({ serverViewId: null }));
vi.mock("./storage", () => ({ serverLocalStorage: {} }));
import {
  createMutationJournal,
  pendingMonitorInput,
  finishMonitorInput,
} from "./mutationIdentity";

function memoryStorage(): Storage {
  const rows = new Map<string, string>();
  return {
    get length() {
      return rows.size;
    },
    clear: () => rows.clear(),
    key: (index) => [...rows.keys()][index] ?? null,
    getItem: (key) => rows.get(key) ?? null,
    setItem: (key, value) => {
      rows.set(key, value);
    },
    removeItem: (key) => {
      rows.delete(key);
    },
  };
}

const path = "/api/monitor/input";
const body = { id: "monitor-one", text: "private stdin text\n" };
afterEach(() => vi.unstubAllGlobals());

describe("monitor input receipt journal access", () => {
  it("exposes only the pending request identity after a reload", async () => {
    const storage = memoryStorage();
    const first = (await createMutationJournal("local", storage)(path, body))!;
    first.unknown();
    expect(pendingMonitorInput(storage, body.id)).toEqual({
      requestId: first.requestId,
    });
    expect(storage.length).toBe(1);
  });

  it("releases only an exact current monitor and request identity", async () => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    const first = (await journal(path, body))!;
    first.unknown();
    expect(
      finishMonitorInput(storage, "another-monitor", first.requestId),
    ).toBe(false);
    expect(finishMonitorInput(storage, body.id, "another-request")).toBe(false);
    expect(pendingMonitorInput(storage, body.id)).toEqual({
      requestId: first.requestId,
    });
    expect(finishMonitorInput(storage, body.id, first.requestId)).toBe(true);
    expect(pendingMonitorInput(storage, body.id)).toBeNull();
    expect(finishMonitorInput(storage, body.id, first.requestId)).toBe(false);
    const next = (await journal(path, body))!;
    expect(next.requestId).not.toBe(first.requestId);
    expect(finishMonitorInput(storage, body.id, first.requestId)).toBe(false);
    expect(pendingMonitorInput(storage, body.id)).toEqual({
      requestId: next.requestId,
    });
  });

  it("keeps receipt access inside its supplied server storage scope", async () => {
    const local = memoryStorage();
    const remote = memoryStorage();
    const first = (await createMutationJournal("local", local)(path, body))!;
    const other = (await createMutationJournal("remote", remote)(path, body))!;
    expect(finishMonitorInput(remote, body.id, first.requestId)).toBe(false);
    expect(pendingMonitorInput(remote, body.id)).toEqual({
      requestId: other.requestId,
    });
    expect(finishMonitorInput(local, body.id, first.requestId)).toBe(true);
    expect(pendingMonitorInput(remote, body.id)).toEqual({
      requestId: other.requestId,
    });
  });

  it("preserves malformed pending records and refuses receipt access", async () => {
    const storage = memoryStorage();
    const first = (await createMutationJournal("local", storage)(path, body))!;
    const key = storage.key(0)!;
    storage.setItem(key, "invalid pending row");
    expect(() => pendingMonitorInput(storage, body.id)).toThrow(
      "monitor input journal",
    );
    expect(() => finishMonitorInput(storage, body.id, first.requestId)).toThrow(
      "monitor input journal",
    );
    expect(storage.getItem(key)).toBe("invalid pending row");
  });
});

describe("durable monitor input journal", () => {
  it("retains the same local identity on HTTP without subtle", async () => {
    const storage = memoryStorage();
    const first = (await createMutationJournal(null, storage)(path, body))!;
    first.unknown();
    vi.stubGlobal("crypto", {
      getRandomValues: webcrypto.getRandomValues.bind(webcrypto),
    });
    vi.stubGlobal(
      "fetch",
      async () =>
        new Response(
          readFileSync(
            new URL("../wasm/generated/client_bg.wasm", import.meta.url),
          ),
          { headers: { "Content-Type": "application/wasm" } },
        ),
    );
    expect(
      (await createMutationJournal(null, storage)(path, body))!.requestId,
    ).toBe(first.requestId);
  });

  it("treats an unresolved saved attempt as uncertain after a reload", async () => {
    const storage = memoryStorage();
    const first = (await createMutationJournal("local", storage)(path, body))!;
    const retry = (await createMutationJournal("local", storage)(path, body))!;
    retry.reject(400, {
      error: "A later preflight cannot prove the prior result",
    });
    expect(
      (await createMutationJournal("local", storage)(path, body))!.requestId,
    ).toBe(first.requestId);
    expect(JSON.parse(storage.getItem(storage.key(0)!)!).uncertain).toBe(true);
  });

  it("preserves field presence for resize and close input conflicts", async () => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    await journal(path, { id: body.id, rows: 0 });
    await expect(journal(path, { id: body.id })).rejects.toThrow(
      "pending monitor input",
    );
    await expect(
      journal(path, { id: body.id, closeStdin: true }),
    ).rejects.toThrow("pending monitor input");
  });

  it("saves no identity if durable storage fails", async () => {
    const storage = memoryStorage();
    vi.spyOn(storage, "setItem").mockImplementation(() => {
      throw new Error("Storage failed");
    });
    await expect(
      createMutationJournal("local", storage)(path, body),
    ).rejects.toThrow("Storage failed");
    expect(storage.length).toBe(0);
  });

  it("keeps concurrent same-payload callers on one identity", async () => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    const [first, second] = await Promise.all([
      journal(path, body),
      journal(path, body),
    ]);
    expect(first!.requestId).toBe(second!.requestId);
    expect(storage.length).toBe(1);
  });

  it.each([null, "local", "lan-server"])(
    "retains one identity across lost responses and reload (%s)",
    async (owner) => {
      const storage = memoryStorage();
      const first = (await createMutationJournal(owner, storage)(path, body))!;
      expect(first).not.toBeNull();
      first.unknown();
      const retry = (await createMutationJournal(owner, storage)(path, {
        ...body,
      }))!;
      expect(retry.requestId).toBe(first.requestId);
      retry.reject(400, { error: "Previous result is unknown" });
      expect(storage.length).toBe(1);
      retry.finish();
      expect(storage.length).toBe(0);
      const next = (await createMutationJournal(owner, storage)(path, body))!;
      expect(next.requestId).not.toBe(first.requestId);
    },
  );

  it("stores only the secure digest, request identity and uncertainty, never stdin", async () => {
    const storage = memoryStorage();
    const entry = (await createMutationJournal("local", storage)(path, body))!;
    expect(entry).not.toBeNull();
    const key = storage.key(0)!;
    expect(key).not.toContain(body.text);
    const raw = storage.getItem(key)!;
    expect(raw).not.toContain(body.text.trim());
    expect(JSON.parse(raw)).toEqual({
      requestId: entry.requestId,
      payloadDigest: createHash("sha256")
        .update(path + "\n" + JSON.stringify(body))
        .digest("hex"),
      uncertain: false,
    });
  });

  it.each([null, "local", "lan-server"])(
    "refuses changed unresolved content before returning an HTTP identity (%s)",
    async (owner) => {
      const storage = memoryStorage();
      const journal = createMutationJournal(owner, storage);
      const pending = (await journal(path, body))!;
      expect(pending).not.toBeNull();
      pending.unknown();
      await expect(
        journal(path, { ...body, text: "different\n" }),
      ).rejects.toThrow("pending monitor input");
      expect((await journal(path, body))!.requestId).toBe(pending.requestId);
      expect(
        (await journal(path, { ...body, id: "monitor-two" }))!.requestId,
      ).not.toBe(pending.requestId);
    },
  );

  it.each([
    [503, {}],
    [400, { code: "outcome_unknown" }],
    [409, { code: "request_pending" }],
  ] as const)("retains unknown HTTP %i results", async (status, response) => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    const first = (await journal(path, body))!;
    expect(first).not.toBeNull();
    first.reject(status, response);
    expect(JSON.parse(storage.getItem(storage.key(0)!)!).uncertain).toBe(true);
    expect((await journal(path, body))!.requestId).toBe(first.requestId);
  });

  it("releases a proven preflight rejection before any uncertainty", async () => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    const first = (await journal(path, body))!;
    expect(first).not.toBeNull();
    first.reject(400, { error: "Input dimensions are invalid" });
    expect(storage.length).toBe(0);
    expect((await journal(path, body))!.requestId).not.toBe(first.requestId);
  });

  it("does not let an explicit identity bypass the unresolved monitor payload", async () => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    const first = (await journal(path, body, "owned-input-id"))!;
    expect(first.requestId).toBe("owned-input-id");
    first.unknown();
    await expect(
      journal(path, { ...body, closeStdin: true }, "owned-input-id"),
    ).rejects.toThrow("pending monitor input");
    await expect(journal(path, body, "another-input-id")).rejects.toThrow(
      "pending monitor input",
    );
    expect((await journal(path, body))!.requestId).toBe(first.requestId);
  });

  it("does not let stale callbacks restore or remove another operation", async () => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    const first = (await journal(path, body))!;
    expect(first).not.toBeNull();
    const retry = (await journal(path, body))!;
    first.finish();
    retry.unknown();
    expect(storage.length).toBe(0);
    const next = (await journal(path, body))!;
    first.finish();
    retry.unknown();
    expect((await journal(path, body))!.requestId).toBe(next.requestId);
  });

  it("does not clear uncertainty from a concurrent retry after a proven rejection", async () => {
    const storage = memoryStorage();
    const journal = createMutationJournal("local", storage);
    const first = (await journal(path, body))!;
    expect(first).not.toBeNull();
    const retry = (await journal(path, body))!;
    first.unknown();
    retry.reject(400, { error: "Cannot confirm input" });
    expect((await journal(path, body))!.requestId).toBe(first.requestId);
    expect(JSON.parse(storage.getItem(storage.key(0)!)!).uncertain).toBe(true);
  });

  it("refuses a corrupt pending record without replacing it", async () => {
    const storage = memoryStorage();
    await createMutationJournal("local", storage)(path, body);
    expect(storage.length).toBe(1);
    const key = storage.key(0)!;
    storage.setItem(key, "corrupt pending record");
    await expect(
      createMutationJournal("local", storage)(path, body),
    ).rejects.toThrow("monitor input journal");
    expect(storage.getItem(key)).toBe("corrupt pending record");
  });

  it("does not change the local bypass for unrelated endpoints", async () => {
    const storage = memoryStorage();
    expect(
      await createMutationJournal(null, storage)("/api/settings", body),
    ).toBeNull();
    expect(
      await createMutationJournal("local", storage)("/api/settings", body),
    ).toBeNull();
    expect(storage.length).toBe(0);
  });
});
