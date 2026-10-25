import { createHash, webcrypto } from "node:crypto";
import { readFileSync } from "node:fs";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { monitorInputStoreFixture } from "./monitorInputStore.fixture";

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
let fixture: ReturnType<typeof monitorInputStoreFixture>;
beforeEach(() => {
  fixture = monitorInputStoreFixture();
});
afterEach(() => vi.unstubAllGlobals());
const journal = (owner: string | null = "local", storage = memoryStorage()) =>
  createMutationJournal(owner, storage, fixture.store(owner));
const path = "/api/monitor/input";
const body = { id: "monitor-one", text: "private stdin text\n" };

describe("monitor input receipt journal access", () => {
  it("exposes only the pending request identity after a reload", async () => {
    const first = (await journal()(path, body))!;
    await first.unknown();
    expect(await pendingMonitorInput(journal(), body.id)).toEqual({
      requestId: first.requestId,
    });
    expect(fixture.read("local", body.id)).toBeDefined();
  });
  it("releases only an exact current monitor and request identity", async () => {
    const owner = journal();
    const first = (await owner(path, body))!;
    await first.unknown();
    expect(
      await finishMonitorInput(owner, "another-monitor", first.requestId),
    ).toBe(false);
    expect(await finishMonitorInput(owner, body.id, "another-request")).toBe(
      false,
    );
    expect(await pendingMonitorInput(owner, body.id)).toEqual({
      requestId: first.requestId,
    });
    expect(await finishMonitorInput(owner, body.id, first.requestId)).toBe(
      true,
    );
    expect(await pendingMonitorInput(owner, body.id)).toBeNull();
    expect(await finishMonitorInput(owner, body.id, first.requestId)).toBe(
      false,
    );
    const next = (await owner(path, body))!;
    expect(next.requestId).not.toBe(first.requestId);
    expect(await finishMonitorInput(owner, body.id, first.requestId)).toBe(
      false,
    );
    expect(await pendingMonitorInput(owner, body.id)).toEqual({
      requestId: next.requestId,
    });
  });
  it("keeps receipt access inside its immutable server scope", async () => {
    const local = journal("local");
    const remote = journal("remote");
    const first = (await local(path, body))!;
    const other = (await remote(path, body))!;
    expect(await finishMonitorInput(remote, body.id, first.requestId)).toBe(
      false,
    );
    expect(await pendingMonitorInput(remote, body.id)).toEqual({
      requestId: other.requestId,
    });
    expect(await finishMonitorInput(local, body.id, first.requestId)).toBe(
      true,
    );
    expect(await pendingMonitorInput(remote, body.id)).toEqual({
      requestId: other.requestId,
    });
  });
  it("preserves malformed pending records and refuses receipt access", async () => {
    const owner = journal();
    const first = (await owner(path, body))!;
    fixture.corrupt("local", body.id, "invalid pending row");
    await expect(pendingMonitorInput(owner, body.id)).rejects.toThrow(
      "monitor input journal",
    );
    await expect(
      finishMonitorInput(owner, body.id, first.requestId),
    ).rejects.toThrow("monitor input journal");
    expect(fixture.read("local", body.id)).toBe("invalid pending row");
  });
});

describe("durable monitor input journal", () => {
  it("does not restore stale A over unresolved B across a paused transaction", async () => {
    const tabA = journal();
    const tabB = journal();
    const first = (await tabA(path, body))!;
    await first.unknown();
    const pause = fixture.pauseNextCommit();
    const oldRetry = tabA(path, body);
    await pause.started;
    let settled = false;
    const settleA = finishMonitorInput(tabB, body.id, first.requestId).then(
      (result) => {
        settled = true;
        return result;
      },
    );
    await Promise.resolve();
    expect(settled).toBe(false);
    pause.release();
    const retry = (await oldRetry)!;
    expect(await settleA).toBe(true);
    const secondBody = { ...body, text: "second private input\n" };
    const second = (await tabB(path, secondBody))!;
    await second.unknown();
    await retry.unknown();
    await retry.finish();
    expect(await pendingMonitorInput(tabA, body.id)).toEqual({
      requestId: second.requestId,
    });
    expect((await tabA(path, secondBody))!.requestId).toBe(second.requestId);
    await expect(tabA(path, body)).rejects.toThrow("pending monitor input");
  });
  it("retains the same local identity on HTTP without subtle", async () => {
    const first = (await journal(null)(path, body))!;
    await first.unknown();
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
    expect((await journal(null)(path, body))!.requestId).toBe(first.requestId);
  });
  it("treats an unresolved saved attempt as uncertain after a reload", async () => {
    const first = (await journal()(path, body))!;
    const retry = (await journal()(path, body))!;
    await retry.reject(400, {
      error: "A later preflight cannot prove the prior result",
    });
    expect((await journal()(path, body))!.requestId).toBe(first.requestId);
    expect(fixture.read("local", body.id)).toHaveProperty("uncertain", true);
  });
  it("preserves field presence for resize and close input conflicts", async () => {
    const owner = journal();
    await owner(path, { id: body.id, rows: 0 });
    await expect(owner(path, { id: body.id })).rejects.toThrow(
      "pending monitor input",
    );
    await expect(
      owner(path, { id: body.id, closeStdin: true }),
    ).rejects.toThrow("pending monitor input");
  });
  it("saves no identity if the durable transaction aborts", async () => {
    fixture.failNext(new Error("Transaction aborted"));
    await expect(journal()(path, body)).rejects.toThrow("Transaction aborted");
    expect(await pendingMonitorInput(journal(), body.id)).toBeNull();
  });
  it("waits for durable commit before returning the HTTP identity", async () => {
    const pause = fixture.pauseNextCommit();
    let returned = false;
    const pending = journal()(path, body).then((value) => {
      returned = true;
      return value;
    });
    await pause.started;
    expect(returned).toBe(false);
    expect(fixture.read("local", body.id)).toBeUndefined();
    pause.release();
    const entry = (await pending)!;
    expect(fixture.read("local", body.id)).toHaveProperty(
      "requestId",
      entry.requestId,
    );
  });
  it("refuses input when IndexedDB is unavailable, with no preference fallback", async () => {
    vi.stubGlobal("indexedDB", undefined);
    const storage = memoryStorage();
    await expect(
      createMutationJournal("local", storage)(path, body),
    ).rejects.toThrow("IndexedDB is unavailable");
    expect(storage.length).toBe(0);
  });
  it("keeps concurrent same-payload callers on one identity", async () => {
    const [first, second] = await Promise.all([
      journal()(path, body),
      journal()(path, body),
    ]);
    expect(first!.requestId).toBe(second!.requestId);
  });
  it.each([null, "local", "lan-server"])(
    "retains one identity across lost responses and reload (%s)",
    async (scope) => {
      const first = (await journal(scope)(path, body))!;
      await first.unknown();
      const retry = (await journal(scope)(path, { ...body }))!;
      expect(retry.requestId).toBe(first.requestId);
      await retry.reject(400, { error: "Previous result is unknown" });
      expect(await pendingMonitorInput(journal(scope), body.id)).toEqual({
        requestId: first.requestId,
      });
      await retry.finish();
      expect(await pendingMonitorInput(journal(scope), body.id)).toBeNull();
      expect((await journal(scope)(path, body))!.requestId).not.toBe(
        first.requestId,
      );
    },
  );
  it("stores only the secure digest, request identity and uncertainty, never stdin or preferences", async () => {
    const storage = memoryStorage();
    const entry = (await journal("local", storage)(path, body))!;
    const raw = fixture.read("local", body.id);
    expect(JSON.stringify(raw)).not.toContain(body.text.trim());
    expect(raw).toEqual({
      requestId: entry.requestId,
      payloadDigest: createHash("sha256")
        .update(path + "\n" + JSON.stringify(body))
        .digest("hex"),
      uncertain: false,
    });
    expect(storage.length).toBe(0);
  });
  it.each([null, "local", "lan-server"])(
    "refuses changed unresolved content before returning an HTTP identity (%s)",
    async (scope) => {
      const owner = journal(scope);
      const pending = (await owner(path, body))!;
      await pending.unknown();
      await expect(
        owner(path, { ...body, text: "different\n" }),
      ).rejects.toThrow("pending monitor input");
      expect((await owner(path, body))!.requestId).toBe(pending.requestId);
      expect(
        (await owner(path, { ...body, id: "monitor-two" }))!.requestId,
      ).not.toBe(pending.requestId);
    },
  );
  it.each([
    [503, {}],
    [400, { code: "outcome_unknown" }],
    [409, { code: "request_pending" }],
  ] as const)("retains unknown HTTP %i results", async (status, response) => {
    const first = (await journal()(path, body))!;
    await first.reject(status, response);
    expect(fixture.read("local", body.id)).toHaveProperty("uncertain", true);
    expect((await journal()(path, body))!.requestId).toBe(first.requestId);
  });
  it("releases a proven preflight rejection before any uncertainty", async () => {
    const first = (await journal()(path, body))!;
    await first.reject(400, { error: "Input dimensions are invalid" });
    expect(await pendingMonitorInput(journal(), body.id)).toBeNull();
    expect((await journal()(path, body))!.requestId).not.toBe(first.requestId);
  });
  it("does not let an explicit identity bypass the unresolved monitor payload", async () => {
    const owner = journal();
    const first = (await owner(path, body, "owned-input-id"))!;
    expect(first.requestId).toBe("owned-input-id");
    await first.unknown();
    await expect(
      owner(path, { ...body, closeStdin: true }, "owned-input-id"),
    ).rejects.toThrow("pending monitor input");
    await expect(owner(path, body, "another-input-id")).rejects.toThrow(
      "pending monitor input",
    );
    expect((await owner(path, body))!.requestId).toBe(first.requestId);
  });
  it("does not let stale callbacks restore or remove another operation", async () => {
    const first = (await journal()(path, body))!;
    const retry = (await journal()(path, body))!;
    await first.finish();
    await retry.unknown();
    expect(await pendingMonitorInput(journal(), body.id)).toBeNull();
    const next = (await journal()(path, body))!;
    await first.finish();
    await retry.unknown();
    expect((await journal()(path, body))!.requestId).toBe(next.requestId);
  });
  it("does not clear uncertainty from a concurrent retry after a proven rejection", async () => {
    const first = (await journal()(path, body))!;
    const retry = (await journal()(path, body))!;
    await first.unknown();
    await retry.reject(400, { error: "Cannot confirm input" });
    expect((await journal()(path, body))!.requestId).toBe(first.requestId);
    expect(fixture.read("local", body.id)).toHaveProperty("uncertain", true);
  });
  it("refuses a corrupt pending record without replacing it", async () => {
    await journal()(path, body);
    fixture.corrupt("local", body.id, "corrupt pending record");
    await expect(journal()(path, body)).rejects.toThrow(
      "monitor input journal",
    );
    expect(fixture.read("local", body.id)).toBe("corrupt pending record");
  });
  it("does not change the local bypass for unrelated endpoints", async () => {
    const storage = memoryStorage();
    expect(await journal(null, storage)("/api/settings", body)).toBeNull();
    expect(await journal("local", storage)("/api/settings", body)).toBeNull();
    expect(storage.length).toBe(0);
  });
});
