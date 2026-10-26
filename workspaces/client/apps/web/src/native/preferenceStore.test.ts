import { expect, it, vi } from "vitest";
import { openNativePreferences } from "./preferenceStore";

it("gets one native identity and keeps it after an uncertain SQLite reply", async () => {
  let lost = true;
  const values: Record<string, string> = {};
  const request = vi.fn(async (method: string, params: unknown) => {
    if (method === "preferences.raw_all") return { ...values };
    if (method === "system.uuid") return "native-id";
    if (method === "preferences.raw_set") {
      const p = params as { key: string; raw: string };
      values[p.key] = p.raw;
      if (lost) {
        lost = false;
        throw new Error("lost reply");
      }
      return { saved: true, revision: "1" };
    }
    throw new Error(method);
  });
  const storage = await openNativePreferences(
    { request: request as any },
    vi.fn(),
  );
  storage.setItem("theme", "dark");
  await expect(storage.flush()).rejects.toThrow("lost reply");
  await storage.flush();
  expect(
    request.mock.calls.filter(([method]) => method === "system.uuid"),
  ).toHaveLength(1);
  const writes = request.mock.calls.filter(
    ([method]) => method === "preferences.raw_set",
  );
  expect(writes).toHaveLength(2);
  expect(writes[0]).toEqual(writes[1]);
  expect(
    (await openNativePreferences({ request: request as any }, vi.fn())).getItem(
      "theme",
    ),
  ).toBe("dark");
});
