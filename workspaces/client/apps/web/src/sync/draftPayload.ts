// Match the server's canonical payload order for local conflict and journal checks.
// Keep every field and nested value from the browser draft.
export function encodeDraftPayload(value: unknown): string {
  const normalized = JSON.parse(JSON.stringify(value));
  const compareKeys = (a: string, b: string) => {
    const right = b[Symbol.iterator]();
    for (const left of a) {
      const next = right.next();
      if (next.done) return 1;
      const difference = left.codePointAt(0)! - next.value.codePointAt(0)!;
      if (difference) return difference;
    }
    return right.next().done ? 0 : -1;
  };
  const encode = (item: unknown): string => {
    if (Array.isArray(item)) return `[${item.map(encode).join(",")}]`;
    if (item === null) return "null";
    if (typeof item === "object")
      return `{${Object.keys(item)
        .sort(compareKeys)
        .map(
          (key) =>
            `${JSON.stringify(key)}:${encode((item as Record<string, unknown>)[key])}`,
        )
        .join(",")}}`;
    return JSON.stringify(item);
  };
  return encode(normalized);
}
