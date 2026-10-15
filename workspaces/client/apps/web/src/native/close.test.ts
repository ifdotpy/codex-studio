import { afterEach, expect, it, vi } from "vitest";
import { bindNativeCloseFrames, initializeNativeClose } from "./close";
import { guardNativeClose } from "./closeState";

afterEach(() => vi.unstubAllGlobals());
async function setup() {
  const listeners = new Set<(event: MessageEvent) => void>();
  let close!: (event: { payload: { id: number } }) => Promise<void>;
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
    createElement: () => ({ style: {} }),
    body: { append },
  });
  vi.stubGlobal("__TAURI__", {
    core: { invoke },
    event: {
      listen: async (_: string, callback: typeof close) => {
        close = callback;
        return () => {};
      },
    },
  });
  await initializeNativeClose();
  return {
    invoke,
    root,
    append,
    close: () => close({ payload: { id: 1 } }),
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
