import { afterEach, expect, it, vi } from "vitest";
import { guardNativeClose } from "./closeState";
import { initializeNativeHostClose } from "./hostClose";
import { fileURLToPath } from "node:url";
import { checkNativeImportGraph } from "../../scripts/nativeImportGraph.mjs";

const ports = vi.hoisted(() => ({
  listen: vi.fn(),
  host: vi.fn(),
  dispose: vi.fn(),
}));
vi.mock("./hostClient", () => ({
  listenHostEvent: ports.listen,
  nativeHost: ports.host,
  disposeHostClient: ports.dispose,
}));
type Callback = (payload: unknown) => void | Promise<void>;
const releases: Array<() => void> = [];
afterEach(() => {
  for (const release of releases.splice(0).reverse()) release();
  vi.unstubAllGlobals();
});
function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function fixture(inert = false) {
  ports.listen.mockReset();
  ports.host.mockReset();
  ports.dispose.mockReset();
  const calls: string[] = [],
    active = new Map<string, Callback>(),
    callbacks = new Map<string, Callback>(),
    stops: Array<ReturnType<typeof vi.fn>> = [];
  const nodes = new Map<string, ElementMock>();
  class ElementMock {
    id = "";
    role = "";
    textContent = "";
    style = { cssText: "" };
    onclick?: () => void;
    children: ElementMock[] = [];
    removed = false;
    append(child: ElementMock) {
      this.children.push(child);
    }
    remove() {
      this.removed = true;
      nodes.delete(this.id);
    }
  }
  const root = { inert };
  const append = vi.fn((node: ElementMock) => {
    nodes.set(node.id, node);
  });
  vi.stubGlobal("document", {
    documentElement: root,
    getElementById: (id: string) => nodes.get(id) ?? null,
    createElement: () => new ElementMock(),
    body: { append },
  });
  const invoke = vi.fn(
    async (command: string, _params: Record<string, unknown>) => {
      calls.push(command);
    },
  );
  const host = { core: { invoke } };
  ports.host.mockReturnValue(host);
  const register = (topic: string, callback: Callback) => {
    calls.push(`listen:${topic}`);
    active.set(topic, callback);
    callbacks.set(topic, callback);
    const stop = vi.fn(() => {
      active.delete(topic);
    });
    stops.push(stop);
    return stop;
  };
  ports.listen.mockImplementation(async (topic: string, callback: Callback) =>
    register(topic, callback),
  );
  return {
    root,
    append,
    nodes,
    invoke,
    host,
    calls,
    active,
    callbacks,
    stops,
    register,
    emit(topic: string, payload: unknown) {
      const callback = callbacks.get(topic);
      expect(callback, `missing ${topic} listener`).toBeTypeOf("function");
      return Promise.resolve(callback!(payload));
    },
  };
}
async function start() {
  const stop = await initializeNativeHostClose();
  releases.push(stop);
  return stop;
}
function guard(callback: () => Promise<void>) {
  const stop = guardNativeClose(callback);
  releases.push(stop);
  return stop;
}

it("keeps the actual host close graph free of frame transport and browser authority", async () => {
  const report = await checkNativeImportGraph({
    root: fileURLToPath(new URL("../../", import.meta.url)),
    entry: fileURLToPath(new URL("./hostClose.ts", import.meta.url)),
  });
  expect(report.findings).toEqual([]);
  expect(report.modules.some((name) => name.endsWith("native/frame.ts"))).toBe(
    false,
  );
});

it("installs both UI listeners before it announces readiness", async () => {
  const f = fixture(),
    setup = deferred();
  ports.listen.mockImplementation(async (topic: string, callback: Callback) => {
    const stop = f.register(topic, callback);
    if (topic === "studio-close-request") await setup.promise;
    return stop;
  });
  const starting = start();
  await Promise.resolve();
  await Promise.resolve();
  expect(f.invoke).not.toHaveBeenCalled();
  setup.resolve();
  await starting;
  expect(f.calls).toEqual([
    "listen:studio-desktop-error",
    "listen:studio-close-request",
    "close_ready",
  ]);
  expect(f.invoke).toHaveBeenCalledWith("close_ready", {});
});
it("cleans a partial listener setup and never announces readiness", async () => {
  const f = fixture();
  ports.listen.mockImplementation(async (topic: string, callback: Callback) => {
    if (topic === "studio-close-request") throw Error("listener unavailable");
    return f.register(topic, callback);
  });
  await expect(start()).rejects.toThrow("listener unavailable");
  expect(f.active.size).toBe(0);
  expect(f.stops[0]).toHaveBeenCalledOnce();
  expect(f.invoke).not.toHaveBeenCalled();
  await f.emit("studio-desktop-error", { message: "late" });
  expect(f.append).not.toHaveBeenCalled();
});
it("cleans both listeners if host readiness fails", async () => {
  const f = fixture();
  f.invoke.mockRejectedValueOnce(Error("ready not confirmed"));
  await expect(start()).rejects.toThrow("ready not confirmed");
  expect(f.active.size).toBe(0);
  for (const stop of f.stops) expect(stop).toHaveBeenCalledOnce();
  await f.emit("studio-close-request", { id: 1 });
  expect(f.root.inert).toBe(false);
  expect(f.invoke).toHaveBeenCalledTimes(1);
});
it("refuses initialization outside the private host document", async () => {
  fixture();
  ports.host.mockReturnValue(undefined);
  await expect(start()).rejects.toThrow(/native|host/i);
  expect(ports.listen).not.toHaveBeenCalled();
});
it("refuses a second owner without removing the first owner's listeners", async () => {
  const f = fixture();
  await start();
  await expect(initializeNativeHostClose()).rejects.toThrow(/already/);
  expect(f.active.size).toBe(2);
  expect(f.invoke).toHaveBeenCalledTimes(1);
});
it("freezes before save guards and sends one positive response only after they finish", async () => {
  const f = fixture(),
    save = deferred();
  let entered = false;
  guard(async () => {
    entered = true;
    expect(f.root.inert).toBe(true);
    await save.promise;
  });
  const stop = await start(),
    closing = f.emit("studio-close-request", { id: 7, action: "close" });
  expect(entered).toBe(true);
  expect(f.root.inert).toBe(true);
  expect(f.invoke).toHaveBeenCalledTimes(1);
  await f.emit("studio-close-request", { id: 7 });
  expect(f.invoke).toHaveBeenCalledTimes(1);
  save.resolve();
  await closing;
  expect(f.invoke).toHaveBeenLastCalledWith("close_response", {
    id: 7,
    ready: true,
  });
  stop();
  expect(f.root.inert).toBe(true);
  await f.emit("studio-close-request", { id: 7 });
  expect(f.invoke).toHaveBeenCalledTimes(2);
});
it.each([false, true])(
  "restores the exact previous inert value %s when a guard fails",
  async (previous) => {
    const f = fixture(previous);
    guard(async () => {
      throw Error("draft not saved");
    });
    await start();
    await f.emit("studio-close-request", { id: 9, action: "reload" });
    expect(f.root.inert).toBe(previous);
    expect(f.invoke).toHaveBeenLastCalledWith("close_response", {
      id: 9,
      ready: false,
    });
    const alert = f.nodes.get("studio-close-error");
    expect(alert?.role).toBe("alert");
    expect(alert?.textContent).toContain("draft not saved");
    expect(alert?.textContent).toContain("Reload");
    await f.emit("studio-close-request", { id: 9, action: "reload" });
    expect(f.invoke).toHaveBeenCalledTimes(2);
  },
);
it("can handle a later request after an exact declined close", async () => {
  const f = fixture();
  const stopGuard = guard(async () => {
    throw Error("hold");
  });
  await start();
  await f.emit("studio-close-request", { id: 1 });
  stopGuard();
  await f.emit("studio-close-request", { id: 2 });
  expect(f.invoke.mock.calls.slice(1)).toEqual([
    ["close_response", { id: 1, ready: false }],
    ["close_response", { id: 2, ready: true }],
  ]);
});
it("keeps a lost positive response frozen and visible without a contradictory response", async () => {
  const f = fixture();
  const stop = await start();
  f.invoke.mockRejectedValueOnce(Error("response lost"));
  await f.emit("studio-close-request", { id: 13, action: "reload" });
  expect(f.root.inert).toBe(true);
  expect(f.invoke.mock.calls.slice(1)).toEqual([
    ["close_response", { id: 13, ready: true }],
  ]);
  expect(f.nodes.get("studio-close-error")?.textContent).toContain(
    "response lost",
  );
  await f.emit("studio-close-request", { id: 13, action: "reload" });
  expect(f.invoke).toHaveBeenCalledTimes(2);
  stop();
  expect(f.root.inert).toBe(true);
});
it("cleanup removes only UI listeners and lets an active save finish without stale ACK", async () => {
  const f = fixture(),
    save = deferred();
  let completed = false;
  guard(async () => {
    await save.promise;
    completed = true;
  });
  const stop = await start(),
    closing = f.emit("studio-close-request", { id: 3 });
  stop();
  stop();
  expect(f.root.inert).toBe(false);
  expect(f.active.size).toBe(0);
  for (const release of f.stops) expect(release).toHaveBeenCalledOnce();
  expect(ports.dispose).not.toHaveBeenCalled();
  save.resolve();
  await closing;
  expect(completed).toBe(true);
  expect(f.invoke).toHaveBeenCalledTimes(1);
  await f.emit("studio-desktop-error", { message: "stale" });
  expect(f.append).not.toHaveBeenCalled();
});
it("shows a host error as text without freezing or interpreting markup", async () => {
  const f = fixture();
  await start();
  await f.emit("studio-desktop-error", { message: "<b>host failure</b>" });
  const first = f.nodes.get("studio-desktop-error");
  expect(first?.textContent).toBe("<b>host failure</b>");
  expect(first?.role).toBe("alert");
  expect(f.root.inert).toBe(false);
  await f.emit("studio-desktop-error", { message: "replacement" });
  expect(first?.removed).toBe(true);
  const second = f.nodes.get("studio-desktop-error");
  second?.children[0].onclick?.();
  expect(second?.removed).toBe(true);
});
it("refuses malformed or rounded close identities without running guards or responding", async () => {
  const f = fixture();
  const save = vi.fn(async () => {});
  guard(save);
  await start();
  for (const payload of [
    null,
    [],
    { id: 0 },
    { id: -1 },
    { id: 1.5 },
    { id: 9007199254740992 },
    { id: "1" },
    { id: 1, action: "other" },
  ])
    await f.emit("studio-close-request", payload);
  expect(save).not.toHaveBeenCalled();
  expect(f.invoke).toHaveBeenCalledTimes(1);
  expect(f.root.inert).toBe(false);
});
