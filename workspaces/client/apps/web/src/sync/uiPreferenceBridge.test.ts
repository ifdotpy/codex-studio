import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  acknowledge: vi.fn(),
  initialize: vi.fn(),
  isSchemaMismatch: vi.fn(() => false),
  read: vi.fn((_scope?: string) => ({})),
  sessionRead: vi.fn(),
  receive: vi.fn(),
  transportWrite: vi.fn(),
  syncPost: vi.fn(
    async (
      _path: string,
      body: { fields: object },
      _options?: { requestId?: string },
    ) => {
      if (mocks.isSchemaMismatch()) throw new Error("Schema mismatch");
      mocks.transportWrite(body);
      return { fields: body.fields };
    },
  ),
  watch: vi.fn((_listener: () => void) => () => {}),
}));

vi.mock("../api", () => ({
  refreshSession: mocks.sessionRead,
  syncPost: mocks.syncPost,
}));
vi.mock("./uiPreferenceStore", () => ({
  acknowledgePreferenceFields: mocks.acknowledge,
  initializePreferenceMigration: mocks.initialize,
  readPreferenceFields: mocks.read,
  receivePreferenceFields: mocks.receive,
  watchPreferenceWrites: mocks.watch,
}));

import { createUiPreferenceBridge } from "./uiPreferenceBridge";

const storage = {} as Storage;
const settle = async () => {
  for (let index = 0; index < 6; index++) await Promise.resolve();
};

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.clearAllMocks();
  vi.useRealTimers();
  mocks.isSchemaMismatch.mockReturnValue(false);
  mocks.read.mockReturnValue({});
  mocks.syncPost.mockImplementation(async (_path, body) => {
    if (mocks.isSchemaMismatch()) throw new Error("Schema mismatch");
    mocks.transportWrite(body);
    return { fields: body.fields };
  });
  vi.unstubAllGlobals();
});

describe("UI preference projection bridge", () => {
  it("applies each projected remote row once", () => {
    const bridge = createUiPreferenceBridge(storage);
    const stop = bridge.start();
    const projection = {
      user: { fields: { a: { value: "remote", timestamp: 1, writer: "w" } } },
    };
    bridge.receiveProjection(projection);
    bridge.receiveProjection(projection);
    expect(mocks.receive).toHaveBeenCalledOnce();
    expect(mocks.receive).toHaveBeenCalledWith(
      "user",
      projection.user.fields,
      storage,
    );
    stop();
  });

  it("sends local changes once through the gated API without reading a session", async () => {
    let changed = () => {};
    mocks.watch.mockImplementation((listener) => {
      changed = listener;
      return () => {};
    });
    mocks.read.mockReturnValue({});
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);
    vi.stubGlobal("EventSource", vi.fn());
    vi.stubGlobal("crypto", {
      subtle: { digest: vi.fn(async () => new Uint8Array(32).buffer) },
    });
    const bridge = createUiPreferenceBridge(storage);
    const stop = bridge.start();
    bridge.receiveProjection({});
    await settle();
    mocks.syncPost.mockClear();

    mocks.read.mockImplementation((scope) =>
      scope === "user"
        ? { local: { value: "local", timestamp: 2, writer: "w" } }
        : {},
    );
    changed();
    await settle();
    expect(mocks.syncPost).toHaveBeenCalledOnce();
    expect(mocks.syncPost.mock.calls[0][0]).toBe("/api/sync/preferences");
    expect(mocks.syncPost.mock.calls[0][2]).toHaveProperty("requestId");
    expect(mocks.transportWrite).toHaveBeenCalledOnce();
    expect(mocks.sessionRead).not.toHaveBeenCalled();
    expect(fetch).not.toHaveBeenCalled();
    expect(EventSource).not.toHaveBeenCalled();
    stop();
  });

  it("sends nothing while the API schema gate is raised", async () => {
    let changed = () => {};
    mocks.watch.mockImplementation((listener) => {
      changed = listener;
      return () => {};
    });
    mocks.read.mockImplementation((scope) =>
      scope === "user"
        ? { local: { value: "local", timestamp: 2, writer: "w" } }
        : {},
    );
    mocks.isSchemaMismatch.mockReturnValue(true);
    vi.stubGlobal("crypto", {
      subtle: { digest: vi.fn(async () => new Uint8Array(32).buffer) },
    });
    const bridge = createUiPreferenceBridge(storage);
    bridge.start();
    bridge.receiveProjection({});
    changed();
    await settle();
    expect(mocks.syncPost).toHaveBeenCalledOnce();
    expect(mocks.transportWrite).not.toHaveBeenCalled();
  });
});
