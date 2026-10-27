import { describe, expect, it } from "vitest";
import {
  deriveWorkspaceLayout,
  isDuoLayout,
  isCompactTools,
  resolveLayoutLength,
} from "./deriveWorkspaceLayout";
import { layoutQueries, layoutValues } from "./layout-contract.generated";

const base = {
  host: { x: 0, y: 0, width: 1400, height: 800 },
  viewport: { x: 0, y: 0, width: 1600, height: 900 },
  rootFontPx: 16,
  workspaceOpen: true,
  teamPreferred: true,
};
describe("workspace layout", () => {
  it("limits Duo proportions to its measured landscape touch range", () => {
    for (const [width, height] of [
      [951, 669],
      [867, 669],
      [951, 500],
      [951, 699],
      [1023, 669],
    ] as const)
      expect(isDuoLayout({ width, height, primaryCoarse: true })).toBe(true);
    for (const [width, height, primaryCoarse] of [
      [951, 499, true],
      [951, 700, true],
      [1024, 669, true],
      [951, 669, false],
      [1440, 900, false],
      [1024, 768, true],
      [390, 844, true],
      [844, 390, true],
    ] as const)
      expect(isDuoLayout({ width, height, primaryCoarse })).toBe(false);
  });
  it("reserves 544 pixels for the Duo conversation only", () => {
    expect(
      deriveWorkspaceLayout({ ...base, duo: true }).conversationMinWidth,
    ).toBe(544);
    expect(deriveWorkspaceLayout(base).conversationMinWidth).toBe(640);
  });
  it("uses complementary ranges at equality and fractional widths", () => {
    expect(layoutQueries.navigationCompact).toBe(
      `(width < ${layoutValues.navigation.inlineMin}px)`,
    );
    expect(
      deriveWorkspaceLayout({
        ...base,
        viewport: {
          ...base.viewport,
          width: layoutValues.navigation.inlineMin - 0.5,
        },
      }).navigationMode,
    ).toBe("drawer");
    expect(
      deriveWorkspaceLayout({
        ...base,
        viewport: {
          ...base.viewport,
          width: layoutValues.navigation.inlineMin,
        },
      }).navigationMode,
    ).toBe("inline");
  });
  it("uses the measured row instead of the wide viewport for panel fit", () => {
    const min =
      resolveLayoutLength("conversation.min", 16) +
      resolveLayoutLength("workspacePanel.min", 16);
    expect(
      deriveWorkspaceLayout({ ...base, host: { ...base.host, width: min } })
        .panelLayout.mode,
    ).toBe("inline");
    const small = deriveWorkspaceLayout({
      ...base,
      host: { ...base.host, width: min - 0.5 },
    });
    expect(small.panelLayout.mode).toBe("drawer");
    expect(small.panelLayout.width).toBe(0);
  });
  it("gives the panel priority without changing the team preference", () => {
    const result = deriveWorkspaceLayout(base);
    expect(result.panelLayout.mode).toBe("inline");
    expect(result.teamInline).toBe(false);
    expect(base.teamPreferred).toBe(true);
    expect(
      deriveWorkspaceLayout({ ...base, workspaceOpen: false }).teamInline,
    ).toBe(true);
  });
  it("resolves rem from the current root size and rejects invalid preferences", () => {
    expect(resolveLayoutLength("conversation.min", 20)).toBe(
      layoutValues.conversation.min * 20,
    );
    const result = deriveWorkspaceLayout({
      ...base,
      preferredPanelWidthRem: Infinity,
      rootFontPx: 20,
    });
    expect(Number.isFinite(result.panelLayout.width)).toBe(true);
    expect(result.panelLayout.width).toBeLessThanOrEqual(
      base.host.width - layoutValues.conversation.min * 20,
    );
  });
  it("subtracts safe areas once for the 951 by 669 target", () => {
    const input = {
      ...base,
      host: { x: 0, y: 0, width: 951, height: 669 },
      safeInsets: { top: 0, right: 84, bottom: 34, left: 0 },
    };
    expect(deriveWorkspaceLayout(input).usable.width).toBe(867);
    expect(deriveWorkspaceLayout(input).usable.height).toBe(635);
    expect(
      deriveWorkspaceLayout({ ...input, hostExcludesSafeInsets: true }).usable
        .width,
    ).toBe(951);
  });
  it("uses the local tools width and main font independently", () => {
    expect(
      isCompactTools(
        layoutValues.tools.compactContainerMax - 0.5,
        layoutValues.tools.compactFontMin - 1,
      ),
    ).toBe(true);
    expect(
      isCompactTools(
        layoutValues.tools.compactContainerMax,
        layoutValues.tools.compactFontMin - 1,
      ),
    ).toBe(false);
    expect(
      isCompactTools(
        layoutValues.tools.compactContainerMax + 100,
        layoutValues.tools.compactFontMin,
      ),
    ).toBe(true);
  });
});
