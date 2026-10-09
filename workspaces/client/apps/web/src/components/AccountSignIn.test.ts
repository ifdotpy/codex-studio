import { describe, expect, it } from "vitest";
import { deviceCodeCountdownSeconds } from "./AccountSignIn";

describe("device-code countdown", () => {
  it("hides the countdown when the native response has no valid expiry", () => {
    expect(deviceCodeCountdownSeconds(undefined, 100)).toBeNull();
    expect(deviceCodeCountdownSeconds(null, 100)).toBeNull();
    expect(deviceCodeCountdownSeconds(Number.NaN, 100)).toBeNull();
    expect(deviceCodeCountdownSeconds(-1, 100)).toBeNull();
  });

  it("uses an explicit server expiry without inventing a lifetime", () => {
    expect(deviceCodeCountdownSeconds(190, 100)).toBe(90);
    expect(deviceCodeCountdownSeconds(90, 100)).toBe(0);
  });
});
