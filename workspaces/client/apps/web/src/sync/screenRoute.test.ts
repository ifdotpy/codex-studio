import { expect, it } from "vitest";
import { writeScreenRoute } from "./screenRoute";

it("updates only the selected screen parameter", () => {
  const oldLocation = globalThis.location;
  const oldHistory = globalThis.history;
  let url = new URL("https://example.test/?chat=one&background=lead");
  Object.defineProperty(globalThis, "location", {
    configurable: true,
    value: url,
  });
  Object.defineProperty(globalThis, "history", {
    configurable: true,
    value: {
      pushState: (_state: unknown, _unused: string, next: URL) => {
        url = next;
        Object.defineProperty(globalThis, "location", {
          configurable: true,
          value: url,
        });
      },
    },
  });
  try {
    writeScreenRoute("tasks", "worker");
    expect(url.searchParams.get("chat")).toBe("one");
    expect(url.searchParams.get("background")).toBe("lead");
    expect(url.searchParams.get("tasks")).toBe("worker");
    writeScreenRoute("tasks", null);
    expect(url.searchParams.has("tasks")).toBe(false);
    expect(url.searchParams.get("background")).toBe("lead");
  } finally {
    Object.defineProperty(globalThis, "location", {
      configurable: true,
      value: oldLocation,
    });
    Object.defineProperty(globalThis, "history", {
      configurable: true,
      value: oldHistory,
    });
  }
});
