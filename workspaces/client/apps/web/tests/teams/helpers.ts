/** Require a fixture member or measured element before using it. */
export function required<T>(value: T | null | undefined): T {
  if (value === null || value === undefined)
    throw new Error("Missing fixture value");
  return value;
}
