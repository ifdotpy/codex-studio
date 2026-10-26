import { afterEach, expect, it, vi } from "vitest";
import {
  importBrowserProfile,
  type BrowserProfileSnapshot,
} from "./importBrowserProfile.ts";

afterEach(() => vi.unstubAllGlobals());

it("restores reserved preference keys before recording a completed import", async () => {
  const values = new Map<string, string>();
  const storage = {
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
  } satisfies Storage;
  const locks = {
    async query() {
      return { held: [], pending: [] };
    },
    async request<T>(
      name: string,
      options: LockOptions | LockGrantedCallback<T>,
      callback?: LockGrantedCallback<T>,
    ): Promise<T> {
      const granted = typeof options === "function" ? options : callback;
      if (!granted) throw new Error("Missing lock callback in the fixture.");
      return granted({ name, mode: "exclusive" });
    },
  } satisfies LockManager;
  vi.stubGlobal("navigator", { locks } satisfies Pick<Navigator, "locks">);
  vi.stubGlobal("localStorage", storage);
  vi.stubGlobal("indexedDB", {
    databases: async () => [],
  } satisfies Pick<IDBFactory, "databases">);
  const snapshot = {
    version: 1,
    preferences: Object.fromEntries([
      ["__proto__", "saved prototype preference"],
      ["constructor", "saved constructor preference"],
      ["toString", "saved string preference"],
    ]),
    databases: [],
  } satisfies BrowserProfileSnapshot;

  await importBrowserProfile(snapshot, "reserved-keys");

  for (const [key, value] of Object.entries(snapshot.preferences))
    expect(storage.getItem(key), key).toBe(value);
  values.set("__proto__", "edited after import");
  await importBrowserProfile(snapshot, "reserved-keys");
  expect(storage.getItem("__proto__")).toBe("edited after import");
});
