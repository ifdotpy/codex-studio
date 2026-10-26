export type TerminalHeightBounds = {
  preferred: number;
  min: number;
  closedBar: number;
  viewportMaxRatio: number;
};

export function terminalHeight(input: {
  preferred: unknown;
  available: number;
  viewport: number;
  opened: boolean;
  bounds: TerminalHeightBounds;
}) {
  const { bounds } = input;
  const nonnegative = (value: number) =>
    Number.isFinite(value) ? Math.max(0, value) : 0;
  const max = Math.min(
    nonnegative(input.available),
    nonnegative(input.viewport) * bounds.viewportMaxRatio,
  );
  const min = Math.min(bounds.min, max);
  const preferred =
    typeof input.preferred === "number" &&
    Number.isFinite(input.preferred) &&
    input.preferred > 0
      ? input.preferred
      : bounds.preferred;
  return {
    height: input.opened
      ? Math.min(max, Math.max(min, preferred))
      : bounds.closedBar,
    min,
    max,
  };
}
