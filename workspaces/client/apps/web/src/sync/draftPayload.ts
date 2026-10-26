// Match the server's canonical payload order for local conflict and journal checks.
// Keep every field and nested value from the browser draft.
export function encodeDraftPayload(value: unknown): string {
  const normalized = JSON.parse(JSON.stringify(value));
  const compareKeys = (a: string, b: string) => {
    const left = Array.from(a, (char) => char.codePointAt(0) ?? 0),
      right = Array.from(b, (char) => char.codePointAt(0) ?? 0);
    for (let i = 0; i < Math.min(left.length, right.length); i++) {
      const leftPoint = left[i],
        rightPoint = right[i];
      if (leftPoint === undefined || rightPoint === undefined)
        throw new Error("A draft key code point is missing.");
      const difference = leftPoint - rightPoint;
      if (difference) return difference;
    }
    return left.length - right.length;
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
    const encoded = JSON.stringify(item);
    if (encoded === undefined)
      throw new Error(
        "The draft payload contains a value without a JSON encoding.",
      );
    return encoded;
  };
  return encode(normalized);
}
