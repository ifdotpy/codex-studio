import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, it, vi } from "vitest";
import type { NativeServerContext } from "../client";
import { createNativeServerServices } from "../serverServices";
import { readPreferenceWriteStatus } from "../../sync/uiPreferenceStatus";
import {
  NativeServicesProvider,
  useServerFileDialogs,
  useServerStorage,
  useServerOutbox,
  useServerAppPreferences,
  useServerOwner,
} from "../servicesContext";

function storage(context: NativeServerContext) {
  const values = new Map<string, string>();
  const listeners = new Set<(key: string | null) => void>();
  return {
    assertOwner(expected: NativeServerContext) {
      if (expected !== context) throw new Error("Wrong storage owner");
    },
    pending: false,
    error: "",
    revision: "0",
    get length() {
      return values.size;
    },
    key: (index: number) => [...values.keys()][index] ?? null,
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
    clear: () => values.clear(),
    flush: vi.fn(async () => {}),
    subscribe(listener: (key: string | null) => void) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    acceptSnapshot(next: Record<string, string>, _revision: string) {
      values.clear();
      for (const [key, raw] of Object.entries(next)) values.set(key, raw);
      for (const listener of listeners) listener(null);
    },
    acceptValue(key: string, raw: string | null, _revision: string) {
      if (raw === null) values.delete(key);
      else values.set(key, raw);
      for (const listener of listeners) listener(key);
    },
  };
}

function owner(
  id: string,
  reply?: (method: string, params: unknown, id?: string) => Promise<unknown>,
) {
  const calls: { method: string; params: unknown; id?: string }[] = [];
  let identity = 0;
  const context: NativeServerContext = {
    handle: `handle:${id}`,
    serverId: id,
    workspaceId: (id === "local" ? "a" : "b").repeat(32),
    origin: `https://${id}.example.test`,
    credentialId: id === "local" ? null : `credential:${id}`,
    async request<T>(
      method: string,
      params: unknown,
      requestId?: string,
    ): Promise<T> {
      calls.push({ method, params, id: requestId });
      if (reply) return (await reply(method, params, requestId)) as T;
      if (method === "system.uuid") return `native:${id}:${++identity}` as T;
      if (method === "desktop.pickDirectory") return `/picked/${id}` as T;
      if (method === "api.prepareSession") return { ready: true } as T;
      throw new Error(`Unexpected native request: ${method}`);
    },
    async subscribe() {
      return async () => {};
    },
  };
  const preferences = storage(context);
  const server = {
    id,
    origin: `https://${id}.example.test`,
    label: id,
    ...(context.credentialId ? { credentialId: context.credentialId } : {}),
  };
  const services = createNativeServerServices(
    server,
    { context, storage: preferences },
    { report: vi.fn() },
  );
  return { services, server, context, preferences, calls };
}

it("provides every actual App service port without a browser default", async () => {
  const f = owner("local");
  for (const key of [
    "api",
    "sync",
    "resources",
    "transcripts",
    "tokenRates",
    "outbox",
    "useDrafts",
    "activity",
    "accountLimits",
    "watchResourceReads",
    "fileDialogs",
    "fileActions",
    "storage",
    "chatCreations",
    "localDrafts",
    "storageEventKey",
    "recordings",
    "audio",
    "uploads",
    "sessionCosts",
    "sidebarServices",
    "sequences",
  ])
    expect(f.services).toHaveProperty(key);
  await f.services.close();
});

it("keeps a real context consumer callback on its captured server after another screen renders", async () => {
  const first = owner("local");
  const second = owner("remote");
  const callbacks: Array<() => Promise<string | null>> = [];
  function Consumer({ expected }: { expected: typeof first }) {
    callbacks.push(useServerFileDialogs().pickDirectory);
    expect(useServerStorage()).toBe(expected.services.storage);
    expect(useServerOutbox()).toBe(expected.services.outbox);
    expect(useServerOwner()).toBe(expected.services.owner);
    return null;
  }
  renderToStaticMarkup(
    createElement(NativeServicesProvider, {
      services: first.services,
      children: createElement(Consumer, { expected: first }),
    }),
  );
  renderToStaticMarkup(
    createElement(NativeServicesProvider, {
      services: second.services,
      children: createElement(Consumer, { expected: second }),
    }),
  );
  first.server.id = "mutated";
  expect(first.services.owner.id).toBe("local");
  expect(second.services.owner.id).toBe("remote");
  expect(await first.services.context).toMatchObject({
    origin: first.context.origin,
    credentialId: null,
  });
  expect(await second.services.context).toMatchObject({
    origin: second.context.origin,
    credentialId: "credential:remote",
  });
  await expect(callbacks[0]()).resolves.toBe("/picked/local");
  expect(first.calls.map((call) => call.method)).toEqual([
    "desktop.pickDirectory",
  ]);
  expect(second.calls).toEqual([]);
  await first.services.close();
  await second.services.close();
});

it("refuses a mismatched native context before a service can run", () => {
  const f = owner("remote");
  expect(() =>
    createNativeServerServices(
      { id: "local", origin: "https://local.example.test", label: "Local" },
      { context: f.context, storage: f.preferences },
      { report: vi.fn() },
    ),
  ).toThrow(/owner/i);
});

it("holds network mutations behind the original local storage commit", async () => {
  const f = owner("local");
  f.preferences.flush.mockRejectedValue(new Error("local commit unknown"));
  await expect(
    f.services.api.post("/api/rename", { id: "chat", name: "Changed" }),
  ).rejects.toThrow("local commit unknown");
  expect(f.calls).toEqual([]);
});

it("waits for both authoritative host views before the real API consumer can read", async () => {
  let finishPreferences!: (value: unknown) => void;
  const preferences = new Promise<unknown>((resolve) => {
    finishPreferences = resolve;
  });
  const f = owner("local", async (method) => {
    if (method === "preferences.view") return preferences;
    if (method === "uploads.snapshot")
      return {
        workspaceId: "a".repeat(32),
        entries: [],
        tombstones: {},
        revision: "2",
      };
    if (method === "api.request")
      return { status: 200, headers: {}, body: { accounts: [] } };
    throw new Error(`Unexpected method ${method}`);
  });
  let complete = false;
  const read = f.services.api.get("/api/accounts").then((value) => {
    complete = true;
    return value;
  });
  await Promise.resolve();
  expect(complete).toBe(false);
  expect(f.calls.some((call) => call.method === "api.request")).toBe(false);
  finishPreferences({
    values: {},
    fields: { user: {}, server: {} },
    sync: { user: { state: "saved" }, server: { state: "saved" } },
    revision: "2",
  });
  await expect(read).resolves.toEqual({ accounts: [] });
  expect(f.services.sidebarServices.available()).toBe(true);
  await f.services.close();
});

it("rejects a foreign upload view instead of mounting an empty native service", async () => {
  const f = owner("local", async (method) => {
    if (method === "preferences.view")
      return {
        values: {},
        fields: { user: {}, server: {} },
        sync: { user: { state: "saved" }, server: { state: "saved" } },
        revision: "1",
      };
    if (method === "uploads.snapshot")
      return {
        workspaceId: "b".repeat(32),
        entries: [],
        tombstones: {},
        revision: "1",
      };
    throw new Error(method);
  });
  await expect(f.services.ready()).rejects.toThrow(/owner/i);
  expect(f.services.sidebarServices.available()).toBe(false);
  expect(f.calls.some((call) => call.method === "api.request")).toBe(false);
  await f.services.close();
});

it("an old screen callback cannot act after its native owner closes", async () => {
  const f = owner("local");
  const pick = f.services.fileDialogs.pickDirectory;
  await f.services.close();
  await expect(pick()).rejects.toThrow(/closed/i);
  expect(f.calls).toEqual([]);
});

it("refuses storage from another native owner before exposing any port", async () => {
  const first = owner("local");
  const second = owner("remote");
  expect(() =>
    createNativeServerServices(
      first.server,
      {
        context: first.context,
        storage: second.preferences,
      },
      { report: vi.fn() },
    ),
  ).toThrow(/owner/i);
  await first.services.close();
  await second.services.close();
});

it("refuses a captured local draft updater after its owner closes", async () => {
  const f = owner("local");
  const update = f.services.localDrafts.updateLocalDraft;
  await f.services.close();
  await expect(
    update("draft", [], (items: string[]) => [...items, "late"]),
  ).rejects.toThrow(/closed/i);
  expect(f.calls).toEqual([]);
});

it("close waits for an admitted initial host read without making services ready", async () => {
  let complete!: (value: unknown) => void;
  const pending = new Promise<unknown>((resolve) => {
    complete = resolve;
  });
  const f = owner("local", async (method) => {
    if (method === "preferences.view") return pending;
    if (method === "uploads.snapshot")
      return {
        workspaceId: "a".repeat(32),
        entries: [],
        tombstones: {},
        revision: "1",
      };
    throw new Error(method);
  });
  const ready = f.services.ready();
  const rejected = expect(ready).rejects.toThrow(/closed/i);
  let closed = false;
  const closing = f.services.close().then(() => {
    closed = true;
  });
  await Promise.resolve();
  await Promise.resolve();
  expect(closed).toBe(false);
  complete({
    values: {},
    fields: { user: {}, server: {} },
    sync: { user: { state: "saved" }, server: { state: "saved" } },
    revision: "1",
  });
  await rejected;
  await closing;
  expect(f.services.sidebarServices.available()).toBe(false);
});

it("the App preference consumer uses the captured native domain and workspace", async () => {
  const f = owner("local", async (method) => {
    if (method === "system.uuid") return "exact-preference";
    if (method === "preferences.edit")
      return {
        values: { preference: '{"enabled":true}' },
        fields: { user: {}, server: {} },
        sync: { user: { state: "saved" }, server: { state: "saved" } },
        revision: "1",
      };
    throw new Error(method);
  });
  let edit: (() => unknown) | undefined;
  function Consumer() {
    const preferences = useServerAppPreferences();
    expect(preferences.projectionBridge).toBeNull();
    edit = () =>
      preferences.editPreference(
        "preference",
        { enabled: false },
        { enabled: true },
      );
    expect(() => preferences.publishWorkspace("foreign")).toThrow(/workspace/i);
    preferences.publishWorkspace(f.context.workspaceId);
    return null;
  }
  renderToStaticMarkup(
    createElement(NativeServicesProvider, {
      services: f.services,
      children: createElement(Consumer),
    }),
  );
  edit!();
  await f.services.flush();
  expect(f.calls.filter((call) => call.method === "preferences.edit")).toEqual([
    {
      method: "preferences.edit",
      params: {
        key: "preference",
        previous: { enabled: false },
        next: { enabled: true },
      },
      id: "exact-preference",
    },
  ]);
  await f.services.close();
});

it("projects native remote uncertainty to the exact Storage consumed by App", async () => {
  const unknown = {
    state: "error",
    error: "Reply lost",
    uncertain: true,
    requestId: "original-request",
  };
  const f = owner("local", async (method) => {
    if (method === "preferences.view")
      return {
        values: {},
        fields: { user: {}, server: {} },
        revision: "3",
        sync: { user: unknown, server: { state: "saved" } },
      };
    if (method === "uploads.snapshot")
      return {
        workspaceId: "a".repeat(32),
        entries: [],
        tombstones: {},
        revision: "3",
      };
    throw new Error(method);
  });
  await f.services.ready();
  expect(readPreferenceWriteStatus(f.services.storage, "user")).toEqual(
    unknown,
  );
  expect(readPreferenceWriteStatus(f.preferences, "user")).toEqual({
    state: "saved",
  });
  expect(f.calls.map((call) => call.method)).toEqual([
    "preferences.view",
    "uploads.snapshot",
  ]);
  await f.services.close();
});

it("derives the immutable service owner only from its admitted host context", async () => {
  const f = owner("remote");
  expect(f.services.owner).toMatchObject({
    id: f.context.serverId,
    origin: f.context.origin,
    credentialId: f.context.credentialId,
    workspaceId: f.context.workspaceId,
  });
  expect(f.calls).toEqual([]);
  await f.services.close();
});

it.each([
  { origin: "https://another.example.test" },
  { credentialId: "another-credential" },
  { workspaceId: "c".repeat(32) },
])(
  "refuses a descriptor that conflicts with the host identity: %j",
  async (identity) => {
    const f = owner("remote");
    expect(() =>
      createNativeServerServices(
        { ...f.server, ...identity },
        { context: f.context, storage: f.preferences },
        { report: vi.fn() },
      ),
    ).toThrow(/owner|identity/i);
    expect(f.calls).toEqual([]);
    await f.services.close();
  },
);

it("close waits for a pending preview and releases only its captured handle", async () => {
  let complete!: (value: unknown) => void;
  const pending = new Promise<unknown>((resolve) => {
    complete = resolve;
  });
  const f = owner("local", async (method, params) => {
    if (method === "preview.open") return pending;
    if (method === "preview.release") {
      expect(params).toEqual({ handle: "exact-preview" });
      return null;
    }
    throw new Error(`Unexpected preview request: ${method}`);
  });
  const opened = f.services.previews.open({ path: "/projects/file.txt" });
  const observed = opened.catch((error: unknown) => error);
  let closed = false;
  const closing = f.services.close().then(() => {
    closed = true;
  });
  await new Promise<void>((resolve) => setImmediate(resolve));
  const closedBeforeReceipt = closed;
  complete({ handle: "exact-preview", length: 0 });
  await observed;
  await closing;
  expect(closedBeforeReceipt).toBe(false);
  expect(f.calls.filter((call) => call.method === "preview.release")).toEqual([
    {
      method: "preview.release",
      params: { handle: "exact-preview" },
      id: undefined,
    },
  ]);
  await expect(
    f.services.previews.open({ path: "/projects/new.txt" }),
  ).rejects.toThrow(/closed/i);
});

it("close retains an uncertain preview release and does not repeat it", async () => {
  const text = JSON.stringify({
    name: "file.txt",
    mime: "text/plain",
    format: "text",
    text: "sample",
    url: "",
    table: null,
    json: null,
  });
  const f = owner("local", async (method) => {
    if (method === "preview.open")
      return { handle: "held-preview", length: text.length };
    if (method === "preview.read")
      return { text, next: text.length, done: true };
    if (method === "preview.release")
      throw new Error("Preview release is unknown");
    throw new Error(`Unexpected preview request: ${method}`);
  });
  await f.services.previews.open({ path: "/projects/file.txt" });
  const closing = f.services.close();
  await expect(closing).rejects.toThrow(
    "The native server services did not close.",
  );
  expect(f.services.close()).toBe(closing);
  expect(
    f.calls.filter((call) => call.method === "preview.release"),
  ).toHaveLength(1);
});
