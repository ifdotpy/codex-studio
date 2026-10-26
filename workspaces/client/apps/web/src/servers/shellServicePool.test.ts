import { expect, it, vi } from "vitest";
import { createShellServicePool } from "./shellServicePool";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
const owner = (credentialId = "one") => ({
  id: "remote",
  origin: "https://remote.example.test",
  credentialId,
  label: "Remote",
});
const service = (credentialId = "one", ready = Promise.resolve()) => ({
  owner: owner(credentialId),
  ready: vi.fn(() => ready),
  close: vi.fn(async () => {}),
});

it("the shared shell admits a native service only after its exact owner is ready", async () => {
  const opened = deferred<ReturnType<typeof service>>();
  const ready = deferred<void>();
  const value = service("one", ready.promise);
  const changed = vi.fn();
  const pool = createShellServicePool(() => opened.promise, changed, vi.fn());
  pool.reconcile([owner()]);
  expect(pool.snapshot().size).toBe(0);
  opened.resolve(value);
  await Promise.resolve();
  expect(value.ready).toHaveBeenCalledOnce();
  expect(pool.snapshot().size).toBe(0);
  ready.resolve();
  await pool.settled();
  expect(pool.snapshot().get("remote")).toBe(value);
  expect(changed).toHaveBeenCalledOnce();
  await pool.close();
});

it("a late native open cannot replace the same server's new credential owner", async () => {
  const old = deferred<ReturnType<typeof service>>();
  const first = service();
  const second = service("two");
  const pool = createShellServicePool(
    (server) =>
      server.credentialId === "one" ? old.promise : Promise.resolve(second),
    vi.fn(),
    vi.fn(),
  );
  pool.reconcile([owner()]);
  pool.reconcile([owner("two")]);
  old.resolve(first);
  await pool.settled();
  expect(pool.snapshot().get("remote")).toBe(second);
  expect(first.close).toHaveBeenCalledOnce();
  expect(second.close).not.toHaveBeenCalled();
  await pool.close();
  expect(second.close).toHaveBeenCalledOnce();
});

it("removal and root close wait for every late native open and close it once", async () => {
  const old = deferred<ReturnType<typeof service>>();
  const value = service();
  const pool = createShellServicePool(() => old.promise, vi.fn(), vi.fn());
  pool.reconcile([owner()]);
  pool.reconcile([]);
  const closing = pool.close();
  old.resolve(value);
  await closing;
  expect(pool.snapshot().size).toBe(0);
  expect(value.close).toHaveBeenCalledOnce();
  expect(value.ready).not.toHaveBeenCalled();
});

it("retains the existing synchronous browser owner without starting readiness reads", async () => {
  const value = service();
  const open = vi.fn(() => value);
  const pool = createShellServicePool(open, vi.fn(), vi.fn());
  pool.reconcile([owner()]);
  pool.reconcile([{ ...owner(), label: "Renamed" }]);
  expect(pool.snapshot().get("remote")).toBe(value);
  expect(open).toHaveBeenCalledOnce();
  expect(value.ready).not.toHaveBeenCalled();
  await pool.close();
});

it("does not mount a failed native readiness and retains its close failure", async () => {
  const value = service("one", Promise.reject(new Error("view unavailable")));
  value.close.mockRejectedValue(new Error("commit unknown"));
  const failed = vi.fn();
  const pool = createShellServicePool(
    () => Promise.resolve(value),
    vi.fn(),
    failed,
  );
  pool.reconcile([owner()]);
  await pool.settled();
  expect(pool.snapshot().size).toBe(0);
  expect(value.close).toHaveBeenCalledOnce();
  await expect(pool.close()).rejects.toThrow(/close/i);
  expect(failed).toHaveBeenCalled();
});

it("removing one root never closes another root's service for the same server", async () => {
  const first = service();
  const second = service();
  const left = createShellServicePool(() => first, vi.fn(), vi.fn());
  const right = createShellServicePool(() => second, vi.fn(), vi.fn());
  left.reconcile([owner()]);
  right.reconcile([owner()]);
  left.reconcile([]);
  await left.close();
  expect(first.close).toHaveBeenCalledOnce();
  expect(second.close).not.toHaveBeenCalled();
  expect(right.snapshot().get("remote")).toBe(second);
  await right.close();
});

it("a changed immutable workspace cannot reuse an older native owner", async () => {
  const first = {
    ...service(),
    owner: { ...owner(), workspaceId: "a".repeat(32) },
  };
  const second = {
    ...service(),
    owner: { ...owner(), workspaceId: "b".repeat(32) },
  };
  const pool = createShellServicePool(
    (server) =>
      server.workspaceId === first.owner.workspaceId ? first : second,
    vi.fn(),
    vi.fn(),
  );
  pool.reconcile([first.owner]);
  pool.reconcile([second.owner]);
  expect(pool.snapshot().get("remote")).toBe(second);
  expect(first.close).toHaveBeenCalledOnce();
  expect(second.close).not.toHaveBeenCalled();
  await pool.close();
});
