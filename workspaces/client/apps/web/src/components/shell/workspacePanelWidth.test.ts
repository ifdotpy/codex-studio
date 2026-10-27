import { describe, expect, test } from "vitest";
import { panelWidthAfterResize, readPanelWidth } from "./workspacePanelWidth";

describe("Workspace panel width preference", () => {
  test("rejects absent and invalid stored sizes", () => {
    for (const value of [null, "", "NaN", "Infinity", "0", "-10", "{}"])
      expect(readPanelWidth(value)).toBeUndefined();
    expect(readPanelWidth("33.75")).toBe(33.75);
  });
  test("clamps a completed left-edge resize without changing the old preference", () => {
    expect(panelWidthAfterResize(540, 500, 700, 360, 600)).toBe(360);
    expect(panelWidthAfterResize(540, 500, 200, 360, 600)).toBe(600);
    expect(panelWidthAfterResize(540, 500, 450, 360, 600)).toBe(590);
  });
  test("cancels an invalid or no-longer-inline resize", () => {
    expect(panelWidthAfterResize(540, 500, 400, 360, 200)).toBeUndefined();
    expect(panelWidthAfterResize(540, 500, NaN, 360, 600)).toBeUndefined();
  });
});
