import {
  layoutTokens,
  layoutValues,
  type LayoutLengthKey,
} from "./layout-contract.generated";

export interface LayoutRect {
  x: number;
  y: number;
  width: number;
  height: number;
}
export interface SafeInsets {
  top: number;
  right: number;
  bottom: number;
  left: number;
}
export interface PanelLayout {
  mode: "inline" | "drawer";
  width: number;
  minWidth: number;
  maxWidth: number;
  rootFontSize: number;
}
export type PaneName = "sidebar" | "conversation" | "panel";
export type PanePathInput = {
  primaryCoarse: boolean;
  panel: "workspace" | "team" | "terminals" | null;
  sidebarOverride: boolean;
  fullScreen: boolean;
  /** Native hinge division coordinates in CSS pixels, including the status strip. */
  hinge?: {
    status: "closed" | "partial" | "open" | "unknown";
    left?: number;
    width?: number;
  };
  /** The app content frame before either navigation or side panels take space. */
  available?: LayoutRect;
  preferredSidebarWidth?: number;
};
export type PanePathLayout = {
  mode: "single" | "pair";
  visible: PaneName[];
  panes: Record<PaneName, LayoutRect | null>;
  divider: LayoutRect | null;
  panelKind: PanePathInput["panel"];
  canFullScreen: boolean;
};
export interface WorkspaceLayoutInput {
  /** The common row already excludes any sidebar owned by its parent. */
  host: LayoutRect;
  viewport: LayoutRect;
  rootFontPx: number;
  safeInsets?: SafeInsets;
  hostExcludesSafeInsets?: boolean;
  preferredPanelWidthRem?: number;
  workspaceOpen?: boolean;
  teamPreferred?: boolean;
  duo?: boolean;
  panePath?: PanePathInput;
}
/** Measured Duo shell and PWA sizes share one landscape proportions rule. */
export function isDuoLayout(input: {
  width: number;
  height: number;
  primaryCoarse: boolean;
}) {
  return (
    input.primaryCoarse &&
    input.width > input.height &&
    input.height >= layoutValues.duo.heightMin &&
    input.height < layoutValues.duo.heightMax &&
    input.width < layoutValues.duo.widthMax
  );
}
/** The same Duo device profile in either orientation; proportions stay landscape-only. */
export function isDuoSurface(input: {
  width: number;
  height: number;
  primaryCoarse: boolean;
}) {
  return isDuoLayout({
    ...input,
    width: Math.max(input.width, input.height),
    height: Math.min(input.width, input.height),
  });
}
export const finiteNonnegative = (value: number, fallback = 0) =>
  Number.isFinite(value) && value >= 0 ? value : fallback;
export function resolveLayoutLength(key: LayoutLengthKey, rootFontPx: number) {
  const token = layoutTokens[key];
  return (
    token.value *
    (token.unit === "rem"
      ? Number.isFinite(rootFontPx) && rootFontPx > 0
        ? rootFontPx
        : layoutValues.viewport.rootFontDefault
      : 1)
  );
}
export function isCompactTools(containerWidth: number, mainFontPx: number) {
  return (
    containerWidth < layoutValues.tools.compactContainerMax ||
    mainFontPx >= layoutValues.tools.compactFontMin
  );
}
/** One path, with its last screen or last pair visible. Native division regions
 * replace the ordinary pair boundary, never the screen order. */
function derivePanePath(
  input: WorkspaceLayoutInput,
  usable: LayoutRect,
  rootFontSize: number,
): PanePathLayout | null {
  const state = input.panePath;
  const nativeClosed =
    state?.hinge?.status === "closed" &&
    input.viewport.width < layoutValues.duo.widthMax;
  if (
    !state?.primaryCoarse ||
    (!nativeClosed &&
      !isDuoLayout({
        width: Math.max(input.viewport.width, input.viewport.height),
        height: Math.min(input.viewport.width, input.viewport.height),
        primaryCoarse: state.primaryCoarse,
      }))
  )
    return null;
  const frame = state.available ?? usable;
  const portrait = input.viewport.width < input.viewport.height;
  const hasPanel = !!state.panel && !state.sidebarOverride;
  const visible: PaneName[] = hasPanel
    ? ["conversation", "panel"]
    : ["sidebar", "conversation"];
  const contentMin = layoutValues.panePath.contentMin;
  const sidebarMin = resolveLayoutLength("sidebar.min", rootFontSize);
  const conversationMin = portrait
    ? contentMin
    : resolveLayoutLength("duo.conversationMin", rootFontSize);
  let leftWidth = hasPanel
    ? frame.width * layoutValues.panePath.splitRatio
    : Math.max(
        sidebarMin,
        Math.min(
          portrait
            ? layoutValues.panePath.portraitSidebar
            : finiteNonnegative(
                state.preferredSidebarWidth ?? NaN,
                resolveLayoutLength("sidebar.preferred", rootFontSize),
              ),
          frame.width - conversationMin,
        ),
      );
  let gap = 0;
  const partial = state.hinge?.status === "partial";
  if (partial) {
    const hingeLeft = finiteNonnegative(
      state.hinge?.left ?? NaN,
      input.viewport.width * layoutValues.panePath.splitRatio,
    );
    const hingeWidth = finiteNonnegative(state.hinge?.width ?? 0);
    const left = Math.max(frame.x, Math.min(frame.x + frame.width, hingeLeft));
    const right = Math.max(
      left,
      Math.min(frame.x + frame.width, hingeLeft + hingeWidth),
    );
    leftWidth = left - frame.x;
    gap = right - left;
  }
  const rightWidth = frame.width - leftWidth - gap;
  const fits =
    leftWidth >= (hasPanel ? contentMin : sidebarMin) &&
    rightWidth >= (partial || hasPanel ? contentMin : conversationMin);
  const pair =
    (!state.fullScreen || partial) && state.hinge?.status !== "closed" && fits;
  const panes: PanePathLayout["panes"] = {
    sidebar: null,
    conversation: null,
    panel: null,
  };
  if (pair) {
    panes[visible[0]] = { ...frame, width: leftWidth };
    panes[visible[1]] = {
      ...frame,
      x: frame.x + leftWidth + gap,
      width: rightWidth,
    };
  } else {
    const last: PaneName = state.sidebarOverride
      ? "sidebar"
      : state.panel
        ? "panel"
        : "conversation";
    visible.splice(0, visible.length, last);
    panes[last] = { ...frame };
  }
  // Navigation paints through Home; conversation content keeps the safe frame.
  const bottomInset = finiteNonnegative(input.safeInsets?.bottom ?? 0);
  for (const name of ["sidebar", "panel"] as const) {
    const pane = panes[name];
    if (pane) pane.height += bottomInset;
  }
  return {
    mode: pair ? "pair" : "single",
    visible,
    panes,
    divider:
      pair && partial ? { ...frame, x: frame.x + leftWidth, width: gap } : null,
    panelKind: state.panel,
    canFullScreen: !partial,
  };
}
export function deriveWorkspaceLayout(input: WorkspaceLayoutInput) {
  const rootFontSize =
    Number.isFinite(input.rootFontPx) && input.rootFontPx > 0
      ? input.rootFontPx
      : layoutValues.viewport.rootFontDefault;
  const length = (key: LayoutLengthKey) =>
    resolveLayoutLength(key, rootFontSize);
  const safe = input.hostExcludesSafeInsets ? undefined : input.safeInsets;
  const usable = {
    x: finiteNonnegative(input.host.x) + finiteNonnegative(safe?.left ?? 0),
    y: finiteNonnegative(input.host.y) + finiteNonnegative(safe?.top ?? 0),
    width: Math.max(
      0,
      finiteNonnegative(input.host.width) -
        finiteNonnegative(safe?.left ?? 0) -
        finiteNonnegative(safe?.right ?? 0),
    ),
    height: Math.max(
      0,
      finiteNonnegative(input.host.height) -
        finiteNonnegative(safe?.top ?? 0) -
        finiteNonnegative(safe?.bottom ?? 0),
    ),
  };
  const conversationMinWidth = length(
    input.duo ? "duo.conversationMin" : "conversation.min",
  );
  const minWidth = length("workspacePanel.min");
  const maximum = Math.max(
    0,
    Math.min(
      usable.width - conversationMinWidth,
      finiteNonnegative(input.viewport.width) *
        layoutValues.workspacePanel.viewportMaxRatio,
    ),
  );
  const inline = !!input.workspaceOpen && maximum >= minWidth;
  const preferred =
    finiteNonnegative(
      input.preferredPanelWidthRem ?? NaN,
      layoutValues.workspacePanel.preferred,
    ) * rootFontSize;
  const panelLayout: PanelLayout = {
    mode: inline ? "inline" : "drawer",
    width: inline ? Math.max(minWidth, Math.min(preferred, maximum)) : 0,
    minWidth: inline ? minWidth : 0,
    maxWidth: inline ? maximum : 0,
    rootFontSize,
  };
  const remaining = usable.width - panelLayout.width;
  const teamCanInline =
    finiteNonnegative(input.viewport.width) >= layoutValues.team.inlineMin &&
    remaining >= conversationMinWidth + length("team.preferred");
  const teamInline = !!input.teamPreferred && teamCanInline;
  return {
    panePath: derivePanePath(input, usable, rootFontSize),
    usable,
    rootFontSize,
    teamCanInline,
    navigationMode:
      finiteNonnegative(input.viewport.width) <
      layoutValues.navigation.inlineMin
        ? ("drawer" as const)
        : ("inline" as const),
    panelLayout,
    teamInline,
    conversationMinWidth,
  };
}
