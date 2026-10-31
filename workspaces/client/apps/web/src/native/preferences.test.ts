import { expect, it } from "vitest";
import { moveLegacyPreferences } from "./preferences";

class MemoryStorage implements Storage {
  [name: string]: unknown;
  private values = new Map<string, string>();
  constructor(
    initial: Record<string, string> = {},
    private readonly lossy = false,
  ) {
    for (const [key, value] of Object.entries(initial))
      this.values.set(key, value);
  }
  get length() {
    return this.values.size;
  }
  clear() {
    this.values.clear();
  }
  getItem(key: string) {
    return this.lossy ? "other" : (this.values.get(key) ?? null);
  }
  key(index: number) {
    return [...this.values.keys()][index] ?? null;
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

it("clears only the rows that the page storage returns unchanged", async () => {
  const target = new MemoryStorage({ theme: "light" });
  const calls: [string, unknown][] = [];
  await moveLegacyPreferences(
    (async (method: string, params: unknown) => {
      calls.push([method, params]);
      return method === "preferences.legacy"
        ? { theme: "dark", size: "12" }
        : null;
    }) as never,
    target,
    false,
  );
  expect(target.getItem("size")).toBe("12");
  // The existing value wins, so its native row stays.
  expect(target.getItem("theme")).toBe("light");
  expect(calls[1]).toEqual([
    "preferences.legacy_clear",
    { scope: "root", keys: ["size"] },
  ]);
});

it("keeps every row when the read-back differs or the read fails", async () => {
  const calls: string[] = [];
  await moveLegacyPreferences(
    (async (method: string) => {
      calls.push(method);
      return { size: "12" };
    }) as never,
    new MemoryStorage({}, true),
    false,
  );
  await moveLegacyPreferences(
    (async () => {
      throw new Error("locked");
    }) as never,
    new MemoryStorage(),
    false,
  );
  expect(calls).toEqual(["preferences.legacy"]);
});
