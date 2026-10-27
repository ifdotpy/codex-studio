import { expect, it } from "vitest";
import { isDuoDevice } from "./duoDevice";
const unknown = {
  nativeHinge: undefined,
  userAgent: "",
  platform: "",
  maxTouchPoints: 5,
};
it("accepts the native hinge owner and phone browser identities", () => {
  expect(isDuoDevice({ ...unknown, nativeHinge: "partial" })).toBe(true);
  expect(isDuoDevice({ ...unknown, userAgent: "iPhone" })).toBe(true);
  expect(isDuoDevice({ ...unknown, mobile: true })).toBe(true);
});
it("keeps iPad Split View and unknown touch devices outside the Duo profile", () => {
  expect(isDuoDevice({ ...unknown, userAgent: "iPad", mobile: true })).toBe(
    false,
  );
  expect(isDuoDevice({ ...unknown, platform: "MacIntel", mobile: true })).toBe(
    false,
  );
  expect(isDuoDevice(unknown)).toBe(false);
});
