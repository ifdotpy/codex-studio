export type SidebarWidthContract = {
  minimumRem: number;
  preferredRem: number;
  conversationMinRem: number;
  navigationInlineMinPx: number;
};

export function storedSidebarWidth(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value > 0
    ? value
    : null;
}

export function clampSidebarWidth(
  width: number,
  minimum: number,
  maximum: number,
) {
  return Math.min(maximum, Math.max(minimum, width));
}

export function resolveSidebarWidth(input: {
  hostWidth: number;
  rootFontSize: number;
  headerMinimum: number;
  preferred: unknown;
  contract: SidebarWidthContract;
}) {
  const { contract, hostWidth, rootFontSize } = input;
  const minimum = Math.max(
    contract.minimumRem * rootFontSize,
    Math.ceil(storedSidebarWidth(input.headerMinimum) ?? 0),
  );
  const maximum = hostWidth - contract.conversationMinRem * rootFontSize;
  const mode =
    storedSidebarWidth(hostWidth) !== null &&
    storedSidebarWidth(rootFontSize) !== null &&
    hostWidth >= contract.navigationInlineMinPx &&
    maximum >= minimum
      ? ("inline" as const)
      : ("drawer" as const);
  return {
    mode,
    minimum,
    maximum: Math.max(minimum, maximum),
    width:
      mode === "inline"
        ? clampSidebarWidth(
            storedSidebarWidth(input.preferred) ??
              contract.preferredRem * rootFontSize,
            minimum,
            maximum,
          )
        : 0,
  };
}
