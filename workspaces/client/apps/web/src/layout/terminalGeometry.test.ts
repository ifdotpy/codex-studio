import { expect, it } from "vitest";
import { readTerminalDockHeight } from "./terminalGeometry";
it("preserves the measured height across the shell message boundary", () => {
  expect(readTerminalDockHeight(4500.5)).toBe(4500.5);
  expect(readTerminalDockHeight(0)).toBe(0);
});
it("rejects invalid measurements instead of changing the dock height", () => {
  for (const value of [NaN, Infinity, -1, "360", null])
    expect(readTerminalDockHeight(value)).toBeUndefined();
});
