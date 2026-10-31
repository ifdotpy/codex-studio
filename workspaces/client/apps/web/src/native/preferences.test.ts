import { expect, it } from "vitest";
import { moveLegacyPreferences } from "./preferences";

function storage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
  } as unknown as Storage;
}

it("clears only the rows that the page storage returns unchanged", async () => {
  const target = storage({ theme: "light" });
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
  const lossy = {
    getItem: () => "other",
    setItem: () => {},
  } as unknown as Storage;
  await moveLegacyPreferences(
    (async (method: string) => {
      calls.push(method);
      return { size: "12" };
    }) as never,
    lossy,
    false,
  );
  await moveLegacyPreferences(
    (async () => {
      throw new Error("locked");
    }) as never,
    storage(),
    false,
  );
  expect(calls).toEqual(["preferences.legacy"]);
});
