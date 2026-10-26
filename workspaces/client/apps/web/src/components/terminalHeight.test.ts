import { describe, expect, it } from "vitest";
import { terminalHeight } from "./terminalHeight";

const bounds = {
  preferred: 360,
  min: 220,
  closedBar: 38,
  viewportMaxRatio: 0.75,
};
const resolve = (preferred: unknown, available: number, viewport = 900) =>
  terminalHeight({ preferred, available, viewport, opened: true, bounds });

describe("terminal height", () => {
  it("keeps the header and composer space in a short host", () => {
    expect(resolve(900, 142, 320)).toEqual({ height: 142, min: 142, max: 142 });
  });
  it("caps a tall preference at the viewport fraction", () => {
    expect(resolve(900, 800).height).toBe(675);
  });
  it("keeps the preference after a temporary clamp", () => {
    const preferred = 600;
    expect(resolve(preferred, 180).height).toBe(180);
    expect(resolve(preferred, 800).height).toBe(preferred);
  });
  it.each([null, "900", NaN, Infinity, -20, 0])(
    "rejects stored value %s",
    (value) => {
      expect(resolve(value, 800).height).toBe(bounds.preferred);
    },
  );
  it("never supplies an inverted clamp or a negative height", () => {
    expect(resolve(1000, -20, 60)).toEqual({ height: 0, min: 0, max: 0 });
  });
  it("uses the measured root font for the nominal floor and closed bar", () => {
    const large = { ...bounds, min: 275, preferred: 450, closedBar: 47.5 };
    expect(
      terminalHeight({
        preferred: 10,
        available: 800,
        viewport: 900,
        opened: true,
        bounds: large,
      }).height,
    ).toBe(275);
    expect(
      terminalHeight({
        preferred: 800,
        available: 800,
        viewport: 900,
        opened: false,
        bounds: large,
      }).height,
    ).toBe(47.5);
  });
});
