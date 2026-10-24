import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { PreferenceFields } from "./uiPreferenceMerge";
import type { syncPost } from "../api";
import { createHash, webcrypto } from "node:crypto";
import { readFileSync } from "node:fs";
import { canonicalPreferenceJson } from "./uiPreferenceMerge";

const mocks = vi.hoisted(() => ({
  acknowledge: vi.fn(),
  resume: vi.fn((_listener: () => void) => () => {}),
  initialize: vi.fn(),
  isSchemaMismatch: vi.fn(() => false),
  read: vi.fn((_scope?: string) => ({})),
  sessionRead: vi.fn(),
  receive: vi.fn(),
  transportWrite: vi.fn(),
  syncPost: vi.fn<typeof syncPost<"/api/sync/preferences">>(
    async (_path, body) => {
      if (mocks.isSchemaMismatch()) throw new Error("Schema mismatch");
      mocks.transportWrite(body);
      return { fields: body.fields ?? {} };
    },
  ),
  watch: vi.fn((_listener: () => void) => () => {}),
}));

vi.mock("../api", () => ({
  refreshSession: mocks.sessionRead,
  syncPost: mocks.syncPost,
}));
vi.mock("./resume", () => ({ onResume: mocks.resume }));
vi.mock("./uiPreferenceStore", () => ({
  acknowledgePreferenceFields: mocks.acknowledge,
  initializePreferenceMigration: mocks.initialize,
  readPreferenceFields: mocks.read,
  receivePreferenceFields: mocks.receive,
  watchPreferenceWrites: mocks.watch,
}));

import { createUiPreferenceBridge } from "./uiPreferenceBridge";

const storage = {} as Storage;

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
    return { fields: body.fields ?? {} };
  });
  vi.unstubAllGlobals();
});

describe("UI preference projection bridge", () => {
  it("syncs HTTP preference edits without subtle and retains the exact retry identity", async () => {
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
    const fields = { local: { value: "Привет 🦀", timestamp: 2, writer: "w" } };
    mocks.read.mockImplementation((scope) => (scope === "user" ? fields : {}));
    mocks.syncPost.mockRejectedValueOnce(new TypeError("Lost response"));
    const bridge = createUiPreferenceBridge(storage, mocks.syncPost);
    const stop = bridge.start();
    try {
      bridge.receiveProjection({});
      await vi.waitFor(() => expect(mocks.syncPost).toHaveBeenCalledOnce());
      expect(mocks.acknowledge).not.toHaveBeenCalled();
      bridge.retryPendingWrites();
      await vi.waitFor(() => expect(mocks.syncPost).toHaveBeenCalledTimes(2));
      const digest = createHash("sha256")
        .update(canonicalPreferenceJson({ scope: "user", fields }))
        .digest("hex");
      expect(mocks.syncPost.mock.calls[0]).toEqual([
        "/api/sync/preferences",
        { scope: "user", fields },
        { requestId: `ui-preferences:${digest}` },
      ]);
      expect(mocks.syncPost.mock.calls[1]).toEqual(
        mocks.syncPost.mock.calls[0],
      );
      expect(mocks.acknowledge).toHaveBeenCalledOnce();
    } finally {
      stop();
    }
  });

  it("applies each projected remote row once", () => {
    const bridge = createUiPreferenceBridge(storage, mocks.syncPost);
    const stop = bridge.start();
    const projection = {
      user: {
        fields: {
          z: { value: "last", timestamp: 2, writer: "w" },
          a: { value: "first", timestamp: 1, writer: "w" },
        },
      },
    };
    bridge.receiveProjection(projection);
    bridge.receiveProjection({
      user: {
        fields: { a: projection.user.fields.a, z: projection.user.fields.z },
      },
    });
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
    const bridge = createUiPreferenceBridge(storage, mocks.syncPost);
    const stop = bridge.start();
    bridge.receiveProjection({});
    mocks.syncPost.mockClear();

    mocks.read.mockImplementation((scope) =>
      scope === "user"
        ? { local: { value: "local", timestamp: 2, writer: "w" } }
        : {},
    );
    changed();
    await vi.waitFor(() => expect(mocks.syncPost).toHaveBeenCalledOnce());
    expect(mocks.syncPost.mock.calls[0][0]).toBe("/api/sync/preferences");
    expect(mocks.syncPost.mock.calls[0][2]).toHaveProperty("requestId");
    expect(mocks.transportWrite).toHaveBeenCalledOnce();
    expect(mocks.sessionRead).not.toHaveBeenCalled();
    expect(fetch).not.toHaveBeenCalled();
    expect(EventSource).not.toHaveBeenCalled();
    stop();
  });

  it("does not resend acknowledged fields when the server reorders keys", async () => {
    let changed = () => {};
    const local = {
      z: { value: "last", timestamp: 2, writer: "w" },
      a: { value: "first", timestamp: 1, writer: "w" },
    };
    const serverOrder = {
      a: local.a,
      z: local.z,
    };
    mocks.watch.mockImplementation((listener) => {
      changed = listener;
      return () => {};
    });
    mocks.read.mockImplementation((scope) => (scope === "user" ? local : {}));
    mocks.acknowledge.mockImplementation(() => changed());
    let posts = 0;
    mocks.syncPost.mockImplementation(async () => {
      posts += 1;
      if (posts > 3) throw new Error("bounded repeated preference write");
      return { fields: serverOrder };
    });
    vi.stubGlobal("crypto", {
      subtle: { digest: vi.fn(async () => new Uint8Array(32).buffer) },
    });
    const bridge = createUiPreferenceBridge(storage, mocks.syncPost);
    const stop = bridge.start();
    bridge.receiveProjection({});
    changed();

    await vi.waitFor(() => expect(mocks.syncPost).toHaveBeenCalledOnce());
    await vi.advanceTimersByTimeAsync(0);
    expect(mocks.syncPost).toHaveBeenCalledOnce();
    stop();
  });

  it("uses the projected server API and storage for preference writes", async () => {
    const pairedStorage = {} as Storage;
    const pairedPost = vi.fn(
      async (_path: string, body: { fields?: PreferenceFields }) => ({
        fields: body.fields ?? {},
      }),
    );
    mocks.read.mockImplementation((scope) =>
      scope === "server"
        ? { local: { value: "paired edit", timestamp: 2, writer: "w" } }
        : {},
    );
    vi.stubGlobal("crypto", {
      subtle: { digest: vi.fn(async () => new Uint8Array(32).buffer) },
    });
    const bridge = createUiPreferenceBridge(pairedStorage, pairedPost);
    const stop = bridge.start();
    bridge.receiveProjection({});
    await vi.waitFor(() => expect(pairedPost).toHaveBeenCalledOnce());
    expect(mocks.syncPost).not.toHaveBeenCalled();
    expect(mocks.read).toHaveBeenCalledWith("server", pairedStorage);
    stop();
  });

  it.each(["resume", "connection"] as const)(
    "retries offline edits through the same API after %s recovery",
    async (recovery) => {
      let resume = () => {};
      const stopResume = vi.fn();
      mocks.resume.mockImplementation((listener) => {
        resume = listener;
        return stopResume;
      });
      mocks.read.mockImplementation((scope) =>
        scope === "server"
          ? { local: { value: "offline edit", timestamp: 2, writer: "w" } }
          : {},
      );
      mocks.syncPost.mockRejectedValueOnce(new TypeError("offline"));
      vi.stubGlobal("crypto", {
        subtle: { digest: vi.fn(async () => new Uint8Array(32).buffer) },
      });
      const bridge = createUiPreferenceBridge(storage, mocks.syncPost);
      const stop = bridge.start();
      bridge.receiveProjection({});
      await vi.advanceTimersByTimeAsync(0);
      expect(mocks.syncPost).toHaveBeenCalledOnce();
      expect(mocks.acknowledge).not.toHaveBeenCalled();
      if (recovery === "resume") resume();
      else bridge.retryPendingWrites();
      await vi.advanceTimersByTimeAsync(0);
      expect(mocks.syncPost).toHaveBeenCalledTimes(2);
      expect(mocks.syncPost.mock.calls[1]).toEqual(
        mocks.syncPost.mock.calls[0],
      );
      expect(mocks.acknowledge).toHaveBeenCalledOnce();
      stop();
      expect(stopResume).toHaveBeenCalledOnce();
      resume();
      bridge.retryPendingWrites();
      await vi.advanceTimersByTimeAsync(0);
      expect(mocks.syncPost).toHaveBeenCalledTimes(2);
    },
  );

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
    const bridge = createUiPreferenceBridge(storage, mocks.syncPost);
    bridge.start();
    bridge.receiveProjection({});
    changed();
    await vi.waitFor(() => expect(mocks.syncPost).toHaveBeenCalledOnce());
    expect(mocks.transportWrite).not.toHaveBeenCalled();
  });
});
