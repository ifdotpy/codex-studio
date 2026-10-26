import { expect, it, vi } from "vitest";
import { openNativePreferences } from "../preferenceStore";
import type { NativeServerContext, NativeSubscriptionEvent } from "../client";
import { createNativeServiceLifecycle } from "./lifecycle";
import { createNativePreferenceDomain } from "./preferenceDomain";
import { createNativeUploads } from "./uploads";

function fixture(
  reply: (method: string, params: unknown, id?: string) => Promise<unknown>,
) {
  const calls: { method: string; params: unknown; id?: string }[] = [];
  const listeners = new Map<string, (value: unknown) => void>();
  let next = 0;
  const context: NativeServerContext = {
    handle: "owner",
    serverId: "local",
    workspaceId: "a".repeat(32),
    origin: "https://local.example.test",
    credentialId: null,
    async request<T>(method: string, params: unknown, id?: string) {
      calls.push({ method, params, id });
      if (method === "preferences.raw_all") return {} as T;
      if (method === "system.uuid") return `write:${++next}` as T;
      return (await reply(method, params, id)) as T;
    },
    async subscribe<T>(
      topic: string,
      _params: Record<string, unknown>,
      accept: (event: NativeSubscriptionEvent<T>) => void,
    ) {
      listeners.set(topic, (value) => accept({ value: value as T }));
      return async () => {
        listeners.delete(topic);
      };
    },
  };
  const report = vi.fn();
  const lifecycle = createNativeServiceLifecycle(context, report);
  return { context, calls, listeners, report, lifecycle };
}
const preferences = (revision: string, raw = "{}") => ({
  revision,
  values: { sidebar: raw },
  fields: { user: {}, server: {} },
  sync: { user: { state: "saved" }, server: { state: "saved" } },
});
const uploads = (
  revision: string,
  tombstones: Record<string, { cancelled: boolean; settled: boolean }> = {},
) => ({
  revision,
  workspaceId: "a".repeat(32),
  entries: [],
  tombstones,
});

it("sends only the native domain edit and lets the host select adoption fields", async () => {
  const f = fixture(async (method) => {
    if (method === "preferences.view") return preferences("1");
    if (method === "preferences.edit") return preferences("2", '{"open":true}');
    if (method === "preferences.adopt_fields")
      return preferences("3", '{"open":true}');
    throw new Error(method);
  });
  const storage = await openNativePreferences(f.context, f.report);
  const domain = createNativePreferenceDomain(
    f.context,
    storage,
    f.lifecycle,
    f.report,
    vi.fn(),
  );
  await domain.initialize();
  expect(
    domain.editPreference("sidebar", { open: false }, { open: true }),
  ).toEqual({ open: true });
  await domain.flush();
  domain.adoptPreferenceFields({});
  await domain.flush();
  expect(f.calls.filter((call) => call.method === "preferences.edit")).toEqual([
    {
      method: "preferences.edit",
      params: {
        key: "sidebar",
        previous: { open: false },
        next: { open: true },
      },
      id: "write:1",
    },
  ]);
  expect(
    f.calls.find((call) => call.method === "preferences.adopt_fields")?.params,
  ).toEqual({});
  expect(storage.getItem("sidebar")).toBe('{"open":true}');
  domain.stop();
  await f.lifecycle.dispose();
});

it("keeps an exact domain write identity after a malformed revision receipt", async () => {
  let attempts = 0;
  const f = fixture(async (method) => {
    if (method === "preferences.view") return preferences("1");
    if (method === "preferences.edit")
      return ++attempts === 1
        ? { ...preferences("2"), revision: undefined }
        : preferences("2");
    throw new Error(method);
  });
  const storage = await openNativePreferences(f.context, f.report);
  const domain = createNativePreferenceDomain(
    f.context,
    storage,
    f.lifecycle,
    f.report,
    vi.fn(),
  );
  await domain.initialize();
  domain.editPreference("sidebar", {}, { open: true });
  await expect(domain.flush()).rejects.toThrow(/revision/i);
  await domain.flush();
  const writes = f.calls.filter((call) => call.method === "preferences.edit");
  expect(writes).toHaveLength(2);
  expect(writes[1]).toEqual(writes[0]);
  expect(f.calls.filter((call) => call.method === "system.uuid")).toHaveLength(
    1,
  );
  await f.lifecycle.dispose();
});

it("does not replace a newer preference event with an older native edit receipt", async () => {
  let finish!: (value: unknown) => void;
  const pending = new Promise<unknown>((resolve) => {
    finish = resolve;
  });
  const f = fixture(async (method) => {
    if (method === "preferences.view") return preferences("1");
    if (method === "preferences.edit") return pending;
    throw new Error(method);
  });
  const storage = await openNativePreferences(f.context, f.report);
  const presentStatus = vi.fn();
  const domain = createNativePreferenceDomain(
    f.context,
    storage,
    f.lifecycle,
    f.report,
    presentStatus,
  );
  await domain.initialize();
  domain.editPreference("sidebar", {}, { open: true });
  const flushed = domain.flush();
  const peer = {
    ...preferences("9", '{"peer":true}'),
    sync: {
      user: {
        state: "error",
        error: "Reply lost",
        uncertain: true,
        requestId: "original-request",
      },
      server: { state: "saved" },
    },
  };
  f.listeners.get("preferences")!(peer);
  finish(preferences("8", '{"open":true}'));
  await flushed;
  expect(storage.getItem("sidebar")).toBe('{"peer":true}');
  expect(presentStatus).toHaveBeenLastCalledWith(peer.sync);
  await f.lifecycle.dispose();
});

it("confirms a lost cancellation response only with the exact host tombstone", async () => {
  let reads = 0;
  const f = fixture(async (method) => {
    if (method === "uploads.snapshot")
      return ++reads === 1
        ? uploads("1")
        : uploads("2", { exact: { cancelled: true, settled: false } });
    if (method === "uploads.cancel") throw new Error("reply lost");
    throw new Error(method);
  });
  const service = createNativeUploads(f.context, f.lifecycle, f.report);
  expect(() => service.uploadCancelled("exact")).toThrow(/wait/i);
  await service.initialize();
  await service.removeUpload("exact");
  expect(service.attachmentCancelled("exact")).toBe(true);
  await expect(service.removeUpload("other")).rejects.toThrow("reply lost");
  expect(f.calls.filter((call) => call.method === "uploads.cancel")).toEqual([
    {
      method: "uploads.cancel",
      params: { id: "exact" },
      id: "upload-cancel:exact",
    },
    {
      method: "uploads.cancel",
      params: { id: "other" },
      id: "upload-cancel:other",
    },
  ]);
  f.listeners.get("uploads")!(uploads("1"));
  expect(service.attachmentCancelled("exact")).toBe(true);
  await service.close();
  await f.lifecycle.dispose();
});
