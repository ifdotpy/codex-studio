/** Required browser fixture data fails at the point the setup omitted it. */
export function required<T>(value: T | null | undefined): T {
  if (value === null || value === undefined)
    throw new Error("Missing required fixture value");
  return value;
}
