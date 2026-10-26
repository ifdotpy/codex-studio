import { expect, it } from "vitest";
import { readNativePreferenceSync } from "./preferenceStatus";
import { readNativePreferenceSnapshot } from "./preferenceDomain";

it("retains exact uncertainty and request identity from the native status contract", () => {
  const value = {
    user: {
      state: "error",
      error: "Reply lost",
      uncertain: true,
      requestId: "original-request",
    },
    server: { state: "saving" },
  };
  expect(readNativePreferenceSync(value)).toEqual(value);
});

it("refuses initial preference readiness without remote status instead of inventing saved", () => {
  expect(() =>
    readNativePreferenceSnapshot({
      values: {},
      fields: { user: {}, server: {} },
      revision: "1",
    }),
  ).toThrow(/status/i);
});

it.each([
  undefined,
  { user: { state: "saved" } },
  { user: { state: "saved", uncertain: true }, server: { state: "saved" } },
  {
    user: { state: "error", error: "Reply lost", uncertain: true },
    server: { state: "saved" },
  },
  {
    user: { state: "error", error: "", uncertain: false, requestId: "id" },
    server: { state: "saved" },
  },
  {
    user: {
      state: "error",
      error: "No reply",
      uncertain: "true",
      requestId: "id",
    },
    server: { state: "saved" },
  },
])("refuses missing or malformed native remote state: %j", (value) => {
  expect(() => readNativePreferenceSync(value)).toThrow(/status/i);
});
