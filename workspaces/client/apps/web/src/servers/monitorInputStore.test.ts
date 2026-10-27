import { afterEach, expect, it, vi } from "vitest";
import {
  createMonitorInputStore,
  MONITOR_INPUT_DATABASE_NAME,
} from "./monitorInputStore";

afterEach(() => vi.unstubAllGlobals());

// Script only browser event delivery. Exercise the real adapter, not a second
// storage implementation. Browser transaction serialization has a Playwright witness.
function databaseEvents() {
  let transactionStarted!: () => void;
  const started = new Promise<void>((resolve) => {
    transactionStarted = resolve;
  });
  const openRequest = {} as IDBOpenDBRequest;
  const readRequest = {} as IDBRequest;
  const store = {
    get: vi.fn(() => readRequest),
    put: vi.fn(),
    delete: vi.fn(),
  };
  const tx = {
    objectStore: vi.fn(() => store),
    abort: vi.fn(),
    error: null as DOMException | null,
  };
  const db = {
    transaction: vi.fn(() => {
      transactionStarted();
      return tx;
    }),
    createObjectStore: vi.fn(),
    close: vi.fn(),
  };
  Object.defineProperty(openRequest, "result", { value: db });
  const open = vi.fn(() => openRequest);
  vi.stubGlobal("indexedDB", { open });
  const fire = (target: object, event: string) => {
    const handler = (target as Record<string, (event: Event) => void>)[
      "on" + event
    ];
    if (!handler) throw new Error("No IndexedDB event handler: " + event);
    handler(new Event(event));
  };
  return {
    open,
    openRequest,
    store,
    tx,
    db,
    fire,
    async connected() {
      fire(openRequest, "success");
      await started;
    },
    read(value: unknown) {
      Object.defineProperty(readRequest, "result", {
        value,
        configurable: true,
      });
      fire(readRequest, "success");
    },
  };
}

const entry = {
  requestId: "request-B",
  payloadDigest: "b".repeat(64),
  uncertain: true,
};

it.each([null, "local", "remote"])(
  "resolves metadata only after transaction completion and keeps the server key (%s)",
  async (scope) => {
    const fixture = databaseEvents();
    let settled = false;
    const pending = createMonitorInputStore(scope).transition(
      "monitor",
      () => ({ entry, result: "committed" }),
    );
    void pending.then(() => {
      settled = true;
    });
    expect(fixture.open).toHaveBeenCalledExactlyOnceWith(
      MONITOR_INPUT_DATABASE_NAME,
      1,
    );
    await fixture.connected();
    expect(fixture.db.transaction).toHaveBeenCalledExactlyOnceWith(
      "inputs",
      "readwrite",
      { durability: "strict" },
    );
    expect(fixture.store.get).toHaveBeenCalledExactlyOnceWith([
      scope || "local",
      "monitor",
    ]);
    fixture.read(undefined);
    expect(fixture.store.put).toHaveBeenCalledExactlyOnceWith(entry, [
      scope || "local",
      "monitor",
    ]);
    await Promise.resolve();
    expect(settled).toBe(false);
    expect(fixture.db.close).not.toHaveBeenCalled();
    fixture.fire(fixture.tx, "complete");
    await expect(pending).resolves.toBe("committed");
    expect(fixture.db.close).toHaveBeenCalledOnce();
  },
);

it("passes the transaction snapshot to the update and requests its exact delete", async () => {
  const fixture = databaseEvents();
  const update = vi.fn((current) => ({ entry: undefined, result: current }));
  const pending = createMonitorInputStore("remote").transition(
    "monitor",
    update,
  );
  await fixture.connected();
  fixture.read(entry);
  expect(update).toHaveBeenCalledExactlyOnceWith(entry);
  expect(fixture.store.delete).toHaveBeenCalledExactlyOnceWith([
    "remote",
    "monitor",
  ]);
  expect(fixture.store.put).not.toHaveBeenCalled();
  expect(fixture.db.close).not.toHaveBeenCalled();
  fixture.fire(fixture.tx, "complete");
  await expect(pending).resolves.toEqual(entry);
  expect(fixture.db.close).toHaveBeenCalledOnce();
});

it.each(["abort", "error"])(
  "rejects a %s event after a write without reporting a committed result",
  async (event) => {
    const fixture = databaseEvents();
    const pending = createMonitorInputStore("local").transition(
      "monitor",
      () => ({ entry, result: "must not be returned" }),
    );
    const rejected = expect(pending).rejects.toThrow("transaction failed");
    await fixture.connected();
    fixture.read(undefined);
    expect(fixture.store.put).toHaveBeenCalledOnce();
    fixture.fire(fixture.tx, event);
    await rejected;
    expect(fixture.db.close).toHaveBeenCalledOnce();
  },
);

it("aborts a malformed transaction snapshot without overwriting the stored row", async () => {
  const fixture = databaseEvents();
  const update = vi.fn(() => ({ entry, result: undefined }));
  const pending = createMonitorInputStore("local").transition(
    "monitor",
    update,
  );
  const rejected = expect(pending).rejects.toThrow("journal is invalid");
  await fixture.connected();
  fixture.read({ requestId: "invalid" });
  expect(update).not.toHaveBeenCalled();
  expect(fixture.tx.abort).toHaveBeenCalledOnce();
  expect(fixture.store.put).not.toHaveBeenCalled();
  expect(fixture.store.delete).not.toHaveBeenCalled();
  fixture.fire(fixture.tx, "abort");
  await rejected;
  expect(fixture.db.close).toHaveBeenCalledOnce();
});

it("keeps the update error when the transaction aborts", async () => {
  const fixture = databaseEvents();
  const failure = new Error("The update refused this input.");
  const pending = createMonitorInputStore("local").transition("monitor", () => {
    throw failure;
  });
  const rejected = expect(pending).rejects.toBe(failure);
  await fixture.connected();
  fixture.read(entry);
  expect(fixture.tx.abort).toHaveBeenCalledOnce();
  fixture.fire(fixture.tx, "abort");
  await rejected;
  expect(fixture.db.close).toHaveBeenCalledOnce();
});

it("closes the connection when a durable transaction cannot start", async () => {
  const fixture = databaseEvents();
  const failure = new Error("The database cannot start a transaction.");
  fixture.db.transaction.mockImplementation(() => {
    throw failure;
  });
  const pending = createMonitorInputStore("local").transition(
    "monitor",
    () => ({ entry, result: undefined }),
  );
  const rejected = expect(pending).rejects.toBe(failure);
  fixture.fire(fixture.openRequest, "success");
  await rejected;
  expect(fixture.db.close).toHaveBeenCalledOnce();
});

it("creates the metadata object store on the database upgrade event", async () => {
  const fixture = databaseEvents();
  const pending = createMonitorInputStore("local").transition(
    "monitor",
    () => ({ entry: undefined, result: undefined }),
  );
  fixture.fire(fixture.openRequest, "upgradeneeded");
  expect(fixture.db.createObjectStore).toHaveBeenCalledExactlyOnceWith(
    "inputs",
  );
  await fixture.connected();
  fixture.read(undefined);
  fixture.fire(fixture.tx, "complete");
  await pending;
});

it("reports a blocked database and closes a late connection without using it", async () => {
  const request = {} as IDBOpenDBRequest;
  vi.stubGlobal("indexedDB", { open: () => request });
  const pending = createMonitorInputStore("local").transition(
    "monitor",
    () => ({ entry: undefined, result: undefined }),
  );
  const rejected = expect(pending).rejects.toThrow("journal is blocked");
  request.onblocked!(new Event("blocked") as IDBVersionChangeEvent);
  await rejected;
  const db = { close: vi.fn(), transaction: vi.fn() };
  Object.defineProperty(request, "result", { value: db });
  request.onsuccess!(new Event("success"));
  expect(db.close).toHaveBeenCalledOnce();
  expect(db.transaction).not.toHaveBeenCalled();
});
