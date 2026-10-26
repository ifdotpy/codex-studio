import { expect, it } from "vitest";
import { readNativeRevision, compareNativeRevisions } from "./revision";

it("compares exact host revisions beyond Number precision", () => {
  expect(compareNativeRevisions("9007199254740993", "9007199254740992")).toBe(
    1,
  );
  expect(compareNativeRevisions("9", "10")).toBe(-1);
  expect(readNativeRevision("9223372036854775807")).toBe("9223372036854775807");
});
it.each([0, 1, "01", "-1", "1.0", "9223372036854775808", undefined])(
  "refuses a noncanonical or out-of-range native revision: %s",
  (value) => {
    expect(() => readNativeRevision(value)).toThrow(/revision/i);
  },
);
