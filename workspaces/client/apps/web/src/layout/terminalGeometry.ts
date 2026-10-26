/** Preserve the effective pixel height reported by the dock. */
export function readTerminalDockHeight(value: unknown): number | undefined {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0)
    return undefined;
  return value;
}
