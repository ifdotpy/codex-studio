import { afterEach, expect, it, vi } from "vitest";
import { bindNativeCloseFrames, initializeNativeClose } from "./close";
import { guardNativeClose } from "./closeState";

afterEach(() => vi.unstubAllGlobals());
async function setup() {
  const listeners = new Set<(event: MessageEvent) => void>();
  let close!: (event: {
    payload: { id: number; action?: "reload" };
  }) => Promise<void>;
  let desktopError!: (event: { payload: { message: string } }) => void;
  const invoke = vi.fn(async () => {});
  const root = { inert: false };
  const append = vi.fn();
  vi.stubGlobal("location", { protocol: "tauri:" });
  vi.stubGlobal("window", {
    addEventListener: (_: string, listener: (event: MessageEvent) => void) =>
      listeners.add(listener),
    removeEventListener: (_: string, listener: (event: MessageEvent) => void) =>
      listeners.delete(listener),
  });
  vi.stubGlobal("document", {
    documentElement: root,
    getElementById: () => null,
    createElement: () => ({ style: {}, append: vi.fn(), remove: vi.fn() }),
    body: { append },
  });
  vi.stubGlobal("__TAURI__", {
    core: { invoke },
    event: {
      listen: async (name: string, callback: typeof close) => {
        if (name === "studio-close-request") close = callback;
        else if (name === "studio-desktop-error")
          desktopError = callback as unknown as typeof desktopError;
        return () => {};
      },
    },
  });
  await initializeNativeClose();
  return {
    invoke,
    desktopError: (message: string) => desktopError({ payload: { message } }),
    root,
    append,
    close: () => close({ payload: { id: 1 } }),
    reload: () => close({ payload: { id: 2, action: "reload" } }),
    message: (event: unknown) => {
      for (const listener of listeners) listener(event as MessageEvent);
    },
  };
}

it("does not acknowledge close until local guards finish", async () => {
  const harness = await setup();
  let done!: () => void;
  const unguard = guardNativeClose(
    () =>
      new Promise<void>((resolve) => {
        done = resolve;
      }),
  );
  try {
    const closing = harness.close();
    expect(harness.root.inert).toBe(true);
    expect(harness.invoke).toHaveBeenCalledTimes(1);
    done();
    await closing;
    expect(harness.invoke).toHaveBeenLastCalledWith("close_response", {
      id: 1,
      ready: true,
    });
  } finally {
    unguard();
  }
});

it("keeps the window editable and reports a failed save", async () => {
  const harness = await setup();
  const unguard = guardNativeClose(async () => {
    throw new Error("disk full");
  });
  try {
    await harness.close();
    expect(harness.root.inert).toBe(false);
    expect(harness.invoke).toHaveBeenLastCalledWith("close_response", {
      id: 1,
      ready: false,
    });
    expect(harness.append.mock.calls[0][0].textContent).toContain("disk full");
  } finally {
    unguard();
  }
});

it("requires an acknowledgement from the exact live server frame", async () => {
  const harness = await setup();
  const source = { postMessage: vi.fn() };
  const frame = {
    src: "studio-view://studio-local.localhost/",
    isConnected: true,
    contentWindow: source,
  };
  const frames = new Map([["local", frame as unknown as HTMLIFrameElement]]);
  const unbind = bindNativeCloseFrames(frames);
  try {
    const closing = harness.close();
    const { id } = source.postMessage.mock.calls[0][0];
    const event = {
      source,
      origin: "studio-view://studio-local.localhost",
      data: { kind: "studio-native-close-reply", id, ready: true },
    };
    harness.message({ ...event, source: {} });
    harness.message({ ...event, origin: "studio-view://other.localhost" });
    await Promise.resolve();
    expect(harness.invoke).toHaveBeenCalledTimes(1);
    // Even a correctly sourced late reply cannot approve a replaced view.
    frames.set("local", { ...frame } as unknown as HTMLIFrameElement);
    harness.message(event);
    await closing;
    expect(harness.invoke).toHaveBeenLastCalledWith("close_response", {
      id: 1,
      ready: false,
    });
    expect(source.postMessage).toHaveBeenLastCalledWith(
      { kind: "studio-native-close-resume", id },
      event.origin,
    );
  } finally {
    unbind();
  }
});

it("reload preserves the save barrier and reports a rejected save", async () => {
  const harness = await setup();
  let reject!: (reason: Error) => void;
  const unguard = guardNativeClose(
    () =>
      new Promise<void>((_, fail) => {
        reject = fail;
      }),
  );
  try {
    const reload = harness.reload();
    expect(harness.root.inert).toBe(true);
    expect(harness.invoke).toHaveBeenCalledTimes(1);
    reject(new Error("draft not saved"));
    await reload;
    expect(harness.invoke).toHaveBeenLastCalledWith("close_response", {
      id: 2,
      ready: false,
    });
    expect(harness.root.inert).toBe(false);
    expect(harness.append.mock.calls[0][0].textContent).toContain(
      "Reload was cancelled",
    );
  } finally {
    unguard();
  }
});

it("shows desktop configuration failures without freezing the editor", async () => {
  const harness = await setup();
  harness.desktopError("The active server prevents this change.");
  expect(harness.root.inert).toBe(false);
  const alert = harness.append.mock.calls[0][0];
  expect(alert.role).toBe("alert");
  expect(alert.textContent).toBe("The active server prevents this change.");
  alert.append.mock.calls[0][0].onclick();
  expect(alert.remove).toHaveBeenCalledOnce();
});
