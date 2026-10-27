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
  const conversationMinWidth = length("conversation.min");
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
