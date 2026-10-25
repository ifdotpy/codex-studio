import { beforeEach, afterEach, expect, it, vi } from "vitest";
import { createUiPreferenceBridge } from "../../sync/uiPreferenceBridge";
import {
  readPreferenceFields,
  receivePreferenceFields,
  preferenceStorageForServer,
} from "../../sync/uiPreferenceStore";
import {
  readPreferenceWriteStatus,
  retryPreferenceWrites,
} from "../../sync/uiPreferenceStatus";
import type { syncPost } from "../../api";
import { readModelWhitelist, writeModelWhitelist } from "./modelWhitelist";

const key = '["studio-model-whitelist-v1"]';
const other = '["codex-project-tree:/state","/repo"]';
function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (i) => [...values.keys()][i] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  };
}
beforeEach(() => {
  vi.stubGlobal("localStorage", memoryStorage());
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("document", new EventTarget());
  vi.stubGlobal(
    "StorageEvent",
    class extends Event {
      constructor(type: string, _options?: unknown) {
        super(type);
      }
    },
  );
});
afterEach(() => vi.unstubAllGlobals());
const expectState = async (
  storage: Storage,
  state: "saved" | "saving" | "error",
) =>
  vi.waitFor(() =>
    expect(readPreferenceWriteStatus(storage, "server").state).toBe(state),
  );

it("persists server-only selections via HTTP, restores an SSE projection after reload, and preserves unrelated settings", async () => {
  const local = preferenceStorageForServer("local");
  const remote = preferenceStorageForServer("remote");
  const fields = {
    [other]: { value: true, timestamp: 1, writer: "other-client" },
  };
  const post = vi.fn<typeof syncPost<"/api/sync/preferences">>(
    async (_path, body) => ({ fields: body.fields ?? {} }),
  );
  const bridge = createUiPreferenceBridge(local, post);
  const stop = bridge.start();
  bridge.receiveProjection({ server: { fields } });
  await expectState(local, "saved");
  writeModelWhitelist(["codex:astra"], local);
  await expectState(local, "saved");
  expect(post).toHaveBeenCalledWith(
    "/api/sync/preferences",
    {
      scope: "server",
      fields: expect.objectContaining({
        [key]: expect.objectContaining({ value: ["codex:astra"] }),
        [other]: fields[other],
      }),
    },
    { requestId: expect.stringMatching(/^ui-preferences:[0-9a-f]{64}$/) },
  );
  expect(readModelWhitelist(remote)).toBeNull();
  expect(readPreferenceFields("server", local)[other]).toEqual(fields[other]);
  const saved = readPreferenceFields("server", local);
  stop();
  local.clear();
  expect(readModelWhitelist(local)).toBeNull();
  const reloaded = createUiPreferenceBridge(local, post);
  const stopReloaded = reloaded.start();
  reloaded.receiveProjection({ server: { fields: saved } });
  expect(readModelWhitelist(local)).toEqual(["codex:astra"]);
  expect(readPreferenceWriteStatus(local, "server").state).toBe("saved");
  stopReloaded();
});

it("shows failed writes and retries an uncertain successful write with the same identity and versions", async () => {
  const storage = preferenceStorageForServer("local");
  let loseResponse = true;
  let committed: ReturnType<typeof readPreferenceFields> = {};
  const post = vi.fn<typeof syncPost<"/api/sync/preferences">>(
    async (_path, body) => {
      committed = { ...committed, ...body.fields };
      if (loseResponse) {
        loseResponse = false;
        throw new Error("Response lost");
      }
      return { fields: committed };
    },
  );
  const bridge = createUiPreferenceBridge(storage, post);
  const stop = bridge.start();
  bridge.receiveProjection({});
  writeModelWhitelist([], storage);
  await expectState(storage, "error");
  expect(readPreferenceWriteStatus(storage, "server")).toEqual({
    state: "error",
    error: expect.stringContaining("Response lost"),
  });
  expect(readModelWhitelist(storage)).toEqual([]);
  expect(committed[key].value).toEqual([]);
  retryPreferenceWrites(storage);
  await expectState(storage, "saved");
  expect(post.mock.calls[1]).toEqual(post.mock.calls[0]);
  expect(readPreferenceWriteStatus(storage, "server").state).toBe("saved");
  stop();
});

it("keeps newer rapid edits while an earlier request is in flight and clock-corrected", async () => {
  const storage = preferenceStorageForServer("local");
  let finish!: (value: {
    fields: ReturnType<typeof readPreferenceFields>;
  }) => void;
  const post = vi
    .fn<typeof syncPost<"/api/sync/preferences">>()
    .mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    )
    .mockImplementation(async (_path, body) => ({ fields: body.fields ?? {} }));
  const bridge = createUiPreferenceBridge(storage, post);
  const stop = bridge.start();
  bridge.receiveProjection({});
  writeModelWhitelist(["codex:astra"], storage);
  await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(1));
  writeModelWhitelist(["codex:astra", "claude:opus"], storage);
  const first = post.mock.calls[0][1].fields!;
  finish({ fields: { ...first, [key]: { ...first[key], timestamp: 1 } } });
  await expectState(storage, "saved");
  expect(post).toHaveBeenCalledTimes(2);
  expect(post.mock.calls[1][2]?.requestId).not.toBe(
    post.mock.calls[0][2]?.requestId,
  );
  expect(readModelWhitelist(storage)).toEqual(["codex:astra", "claude:opus"]);
  expect(readPreferenceWriteStatus(storage, "server").state).toBe("saved");
  stop();
});

it("retains explicit null and none through projected updates, ignoring older server choices", () => {
  const storage = preferenceStorageForServer("local");
  writeModelWhitelist([], storage);
  const field = readPreferenceFields("server", storage)[key];
  receivePreferenceFields(
    "server",
    { [key]: { ...field, timestamp: 0, value: null } },
    storage,
  );
  expect(readModelWhitelist(storage)).toEqual([]);
  writeModelWhitelist(null, storage);
  expect(readPreferenceFields("server", storage)[key].value).toBeNull();
  expect(readModelWhitelist(storage)).toBeNull();
});

it("saves the server whitelist when user preferences are rejected and retries only the failed scope", async () => {
  const storage = preferenceStorageForServer("local");
  const userKey = '["codex-studio-preferences-v1","theme"]';
  receivePreferenceFields(
    "user",
    {
      [userKey]: { value: "dark", timestamp: 1, writer: "other-client" },
    },
    storage,
  );
  writeModelWhitelist(["codex:astra"], storage);
  const post = vi.fn<typeof syncPost<"/api/sync/preferences">>(
    async (_path, body) => {
      if (body.scope === "user") throw new Error("User preferences rejected");
      return { fields: body.fields ?? {} };
    },
  );
  const bridge = createUiPreferenceBridge(storage, post);
  const stop = bridge.start();
  try {
    bridge.receiveProjection({});
    await expectState(storage, "saved");
    expect(readPreferenceWriteStatus(storage, "user")).toEqual({
      state: "error",
      error: "User preferences rejected",
    });
    expect(post.mock.calls.map((call) => call[1].scope)).toEqual([
      "user",
      "server",
    ]);
    expect(readPreferenceFields("user", storage)[userKey].value).toBe("dark");
    expect(readModelWhitelist(storage)).toEqual(["codex:astra"]);
    retryPreferenceWrites(storage);
    await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(3));
    await vi.waitFor(() =>
      expect(readPreferenceWriteStatus(storage, "user").state).toBe("error"),
    );
    expect(post.mock.calls[2]).toEqual(post.mock.calls[0]);
    expect(post.mock.calls.map((call) => call[1].scope)).toEqual([
      "user",
      "server",
      "user",
    ]);
    expect(readPreferenceWriteStatus(storage, "server").state).toBe("saved");
  } finally {
    stop();
  }
});

it.each(["edit", "retry"] as const)(
  "sends a whitelist %s queued after failure while the other scope is in flight",
  async (action) => {
    const storage = preferenceStorageForServer("local");
    let rejectServer!: (error: Error) => void;
    let finishUser!: () => void;
    const post = vi
      .fn<typeof syncPost<"/api/sync/preferences">>()
      .mockImplementationOnce(
        () =>
          new Promise((_resolve, reject) => {
            rejectServer = reject;
          }),
      )
      .mockImplementationOnce(
        (_path, body) =>
          new Promise((resolve) => {
            finishUser = () => resolve({ fields: body.fields ?? {} });
          }),
      )
      .mockImplementation(async (_path, body) => ({
        fields: body.fields ?? {},
      }));
    const bridge = createUiPreferenceBridge(storage, post);
    const stop = bridge.start();
    try {
      bridge.receiveProjection({});
      writeModelWhitelist(["codex:astra"], storage);
      await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(1));
      expect(post.mock.calls[0][1].scope).toBe("server");
      receivePreferenceFields(
        "user",
        {
          '["codex-studio-preferences-v1","theme"]': {
            value: "dark",
            timestamp: 1,
            writer: "other-client",
          },
        },
        storage,
      );
      rejectServer(new Error("Server preference rejected"));
      await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(2));
      expect(post.mock.calls[1][1].scope).toBe("user");
      if (action === "edit") writeModelWhitelist(["claude:opus"], storage);
      else retryPreferenceWrites(storage);
      finishUser();
      await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(3));
      await expectState(storage, "saved");
      expect(post.mock.calls.map((call) => call[1].scope)).toEqual([
        "server",
        "user",
        "server",
      ]);
      if (action === "retry")
        expect(post.mock.calls[2]).toEqual(post.mock.calls[0]);
      else {
        expect(post.mock.calls[2][2]?.requestId).not.toBe(
          post.mock.calls[0][2]?.requestId,
        );
        expect(readModelWhitelist(storage)).toEqual(["claude:opus"]);
      }
    } finally {
      stop();
    }
  },
);
