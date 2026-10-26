import { describe, expect, it } from "vitest";
import { deriveWorkspaceLayout } from "./deriveWorkspaceLayout";

const base = {
  host: { x: 0, y: 0, width: 869, height: 635 },
  viewport: { x: 0, y: 0, width: 951, height: 669 },
  rootFontPx: 16,
  hostExcludesSafeInsets: true,
  duo: true,
};
const state = {
  primaryCoarse: true,
  panel: null as "workspace" | "team" | null,
  sidebarOverride: false,
  fullScreen: false,
  hinge: {
    status: "open" as "open" | "partial" | "closed" | "unknown",
    left: 455.5,
    width: 40,
  },
};
const layout = (patch: Partial<typeof state> = {}) =>
  deriveWorkspaceLayout({ ...base, panePath: { ...state, ...patch } }).panePath;

describe("Duo pane path", () => {
  it("keeps the sidebar narrower than the conversation when fully open", () => {
    const result = layout();
    expect(result?.mode).toBe("pair");
    expect(result?.visible).toEqual(["sidebar", "conversation"]);
    expect(result?.panes.sidebar?.width).toBeCloseTo(316.8);
    expect(result?.panes.conversation?.width).toBeCloseTo(552.2);
    expect(result?.panes.panel).toBeNull();
  });
  it.each(["workspace", "team"] as const)(
    "pairs the conversation with the %s panel at equal widths",
    (panel) => {
      const result = layout({ panel });
      expect(result?.visible).toEqual(["conversation", "panel"]);
      expect(result?.panes.sidebar).toBeNull();
      expect(result?.panes.conversation).toEqual({
        x: 0,
        y: 0,
        width: 434.5,
        height: 635,
      });
      expect(result?.panes.panel).toEqual({
        x: 434.5,
        y: 0,
        width: 434.5,
        height: 635,
      });
      expect(result?.panelKind).toBe(panel);
    },
  );
  it.each([null, "workspace", "team"] as const)(
    "keeps both panes outside the partial-fold band, panel %s",
    (panel) => {
      const result = layout({
        panel,
        hinge: { ...state.hinge, status: "partial" },
      });
      const [left, right] = result!.visible.map((pane) => result!.panes[pane]!);
      expect(left.x + left.width).toBe(455.5);
      expect(right.x).toBe(495.5);
      expect(right.width).toBe(373.5);
      expect(result?.divider).toEqual({
        x: 455.5,
        y: 0,
        width: 40,
        height: 635,
      });
    },
  );
  it("shows only the last screen when closed and keeps the panel identity", () => {
    const result = layout({
      panel: "team",
      hinge: { ...state.hinge, status: "closed" },
    });
    expect(result?.mode).toBe("single");
    expect(result?.visible).toEqual(["panel"]);
    expect(result?.panes.panel).toEqual(base.host);
    expect(result?.panelKind).toBe("team");
  });
  it("keeps the selected panel while the sidebar temporarily replaces it", () => {
    const result = layout({ panel: "team", sidebarOverride: true });
    expect(result?.visible).toEqual(["sidebar", "conversation"]);
    expect(result?.panelKind).toBe("team");
    expect(layout({ panel: "team" })?.visible).toEqual([
      "conversation",
      "panel",
    ]);
  });
  it("uses the last panel in full screen and restores its pair", () => {
    expect(layout({ panel: "workspace", fullScreen: true })?.visible).toEqual([
      "panel",
    ]);
    expect(layout({ panel: "workspace" })?.visible).toEqual([
      "conversation",
      "panel",
    ]);
  });
  it("keeps a 250 pixel portrait sidebar before the conversation", () => {
    const result = deriveWorkspaceLayout({
      ...base,
      host: { x: 0, y: 0, width: 669, height: 869 },
      viewport: { x: 0, y: 0, width: 669, height: 951 },
      panePath: state,
    }).panePath;
    expect(result?.visible).toEqual(["sidebar", "conversation"]);
    expect(result?.panes.sidebar?.width).toBe(250);
    expect(result?.panes.conversation?.width).toBe(419);
  });
  it("uses equal portrait content panes when a side panel is open", () => {
    const result = deriveWorkspaceLayout({
      ...base,
      host: { x: 0, y: 0, width: 669, height: 869 },
      viewport: { x: 0, y: 0, width: 669, height: 951 },
      panePath: { ...state, panel: "team" },
    }).panePath;
    expect(result?.panes.conversation?.width).toBe(334.5);
    expect(result?.panes.panel?.width).toBe(334.5);
  });
  it("leaves desktop and fine primary touch input unchanged", () => {
    expect(layout({ primaryCoarse: false })).toBeNull();
    expect(
      deriveWorkspaceLayout({
        ...base,
        viewport: { ...base.viewport, width: 1200 },
        panePath: state,
      }).panePath,
    ).toBeNull();
    expect(
      deriveWorkspaceLayout({
        ...base,
        host: { ...base.host, width: 390 },
        viewport: { ...base.viewport, width: 390, height: 844 },
        panePath: state,
      }).panePath,
    ).toBeNull();
  });
});
