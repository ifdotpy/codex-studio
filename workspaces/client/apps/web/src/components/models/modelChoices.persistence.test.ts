import { required } from "../componentTestFixtures";
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
import { preferenceCacheKey } from "../../sync/uiPreferenceMerge";
import { readModelChoices, writeModelChoices } from "./modelChoices";

const key = '["studio-model-choices-v1"]';
const astra = { "codex:astra": "whitelisted" } as const;
const hidden = { "codex:astra": "blacklisted" } as const;
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
  writeModelChoices(astra, local);
  await expectState(local, "saved");
  expect(post).toHaveBeenCalledWith(
    "/api/sync/preferences",
    {
      scope: "server",
      fields: expect.objectContaining({
        [key]: expect.objectContaining({ value: astra }),
        [other]: fields[other],
      }),
    },
    { requestId: expect.stringMatching(/^ui-preferences:[0-9a-f]{64}$/) },
  );
  expect(readModelChoices(remote)).toEqual({});
  expect(readPreferenceFields("server", local)[other]).toEqual(fields[other]);
  const saved = readPreferenceFields("server", local);
  stop();
  local.clear();
  expect(readModelChoices(local)).toEqual({});
  const reloaded = createUiPreferenceBridge(local, post);
  const stopReloaded = reloaded.start();
  reloaded.receiveProjection({ server: { fields: saved } });
  expect(readModelChoices(local)).toEqual(astra);
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
  writeModelChoices(hidden, storage);
  await expectState(storage, "error");
  expect(readPreferenceWriteStatus(storage, "server")).toEqual({
    state: "error",
    error: expect.stringContaining("Response lost"),
  });
  expect(readModelChoices(storage)).toEqual(hidden);
  expect(required(committed[key]).value).toEqual(hidden);
  retryPreferenceWrites(storage);
  await expectState(storage, "saved");
  expect(required(post.mock.calls[1])).toEqual(required(post.mock.calls[0]));
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
  writeModelChoices(astra, storage);
  await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(1));
  writeModelChoices({ ...astra, "claude:opus": "blacklisted" }, storage);
  const first = required(required(post.mock.calls[0])[1]).fields!;
  finish({
    fields: { ...first, [key]: { ...required(first[key]), timestamp: 1 } },
  });
  await expectState(storage, "saved");
  expect(post).toHaveBeenCalledTimes(2);
  expect(required(required(post.mock.calls[1])[2])?.requestId).not.toBe(
    required(required(post.mock.calls[0])[2])?.requestId,
  );
  expect(readModelChoices(storage)).toEqual({
    ...astra,
    "claude:opus": "blacklisted",
  });
  expect(readPreferenceWriteStatus(storage, "server").state).toBe("saved");
  stop();
});

it("retains newer choices through projected updates, ignoring older server choices", () => {
  const storage = preferenceStorageForServer("local");
  writeModelChoices(hidden, storage);
  const field = required(readPreferenceFields("server", storage)[key]);
  receivePreferenceFields(
    "server",
    { [key]: { ...field, timestamp: 0, value: {} } },
    storage,
  );
  expect(readModelChoices(storage)).toEqual(hidden);
  writeModelChoices({}, storage);
  expect(required(readPreferenceFields("server", storage)[key]).value).toEqual(
    {},
  );
  expect(readModelChoices(storage)).toEqual({});
});

it("saves the server model choices when user preferences are rejected and retries only the failed scope", async () => {
  const storage = preferenceStorageForServer("local");
  const userKey = '["codex-studio-preferences-v1","theme"]';
  receivePreferenceFields(
    "user",
    {
      [userKey]: { value: "dark", timestamp: 1, writer: "other-client" },
    },
    storage,
  );
  writeModelChoices(astra, storage);
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
    expect(post.mock.calls.map((call) => required(call[1]).scope)).toEqual([
      "user",
      "server",
    ]);
    expect(required(readPreferenceFields("user", storage)[userKey]).value).toBe(
      "dark",
    );
    expect(readModelChoices(storage)).toEqual(astra);
    retryPreferenceWrites(storage);
    await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(3));
    await vi.waitFor(() =>
      expect(readPreferenceWriteStatus(storage, "user").state).toBe("error"),
    );
    expect(required(post.mock.calls[2])).toEqual(required(post.mock.calls[0]));
    expect(post.mock.calls.map((call) => required(call[1]).scope)).toEqual([
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
  "sends a model choice %s queued after failure while the other scope is in flight",
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
      writeModelChoices(astra, storage);
      await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(1));
      expect(required(required(post.mock.calls[0])[1]).scope).toBe("server");
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
      expect(required(required(post.mock.calls[1])[1]).scope).toBe("user");
      if (action === "edit")
        writeModelChoices({ "claude:opus": "blacklisted" }, storage);
      else retryPreferenceWrites(storage);
      finishUser();
      await vi.waitFor(() => expect(post).toHaveBeenCalledTimes(3));
      await expectState(storage, "saved");
      expect(post.mock.calls.map((call) => required(call[1]).scope)).toEqual([
        "server",
        "user",
        "server",
      ]);
      if (action === "retry")
        expect(required(post.mock.calls[2])).toEqual(
          required(post.mock.calls[0]),
        );
      else {
        expect(required(required(post.mock.calls[2])[2])?.requestId).not.toBe(
          required(required(post.mock.calls[0])[2])?.requestId,
        );
        expect(readModelChoices(storage)).toEqual({
          "claude:opus": "blacklisted",
        });
      }
    } finally {
      stop();
    }
  },
);

it("drops a cached retired whitelist so server pushes stay valid", () => {
  const storage = preferenceStorageForServer("local");
  const retired = '["studio-model-whitelist-v1"]';
  storage.setItem(
    `${preferenceCacheKey}:server`,
    JSON.stringify({
      [retired]: { value: ["codex:astra"], timestamp: 1, writer: "old" },
      [other]: { value: true, timestamp: 1, writer: "other-client" },
    }),
  );
  expect(Object.keys(readPreferenceFields("server", storage))).toEqual([other]);
});
