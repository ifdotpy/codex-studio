import { expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  edit: vi.fn(),
  bridge: {
    receiveProjection: vi.fn(),
    start: vi.fn(),
    retryPendingWrites: vi.fn(),
  },
  createBridge: vi.fn(),
}));
vi.mock("../sync/uiPreferenceStore", () => ({ writePreferenceEdit: h.edit }));
vi.mock("../sync/uiPreferenceBridge", () => ({
  createUiPreferenceBridge: h.createBridge,
}));
vi.mock("./registry", () => ({
  SERVER_REGISTRY_EVENT: "studio-server-registry",
}));
import { createBrowserAppPreferences } from "./browserAppPreferences";
import type { syncPost } from "../api";

it("keeps the maintained browser bridge and passes the exact owner storage to edits", () => {
  const values = new Map<string, string>();
  const storage: Storage = {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  };
  const post: typeof syncPost = vi.fn();
  h.createBridge.mockReturnValue(h.bridge);
  const port = createBrowserAppPreferences("remote", storage, post);
  const next = { enabled: true };
  h.edit.mockReturnValue(next);
  expect(port.projectionBridge).toBe(h.bridge);
  expect(h.createBridge).toHaveBeenCalledWith(storage, post);
  expect(port.editPreference("key", { enabled: false }, next)).toBe(next);
  expect(h.edit).toHaveBeenCalledWith("key", { enabled: false }, next, storage);
});
