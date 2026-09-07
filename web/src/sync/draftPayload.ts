// Match the server's JSON payload order, including older servers that compare
// payload strings. Keep every field and nested value from the browser draft.
export function encodeDraftPayload(value: unknown): string {
  const normalized = JSON.parse(JSON.stringify(value));
  const compareKeys = (a: string, b: string) => {
    const left = Array.from(a),
      right = Array.from(b);
    for (let i = 0; i < Math.min(left.length, right.length); i++) {
      const difference = left[i].codePointAt(0)! - right[i].codePointAt(0)!;
      if (difference) return difference;
    }
    return left.length - right.length;
  };
  const encode = (item: any): string => {
    if (Array.isArray(item)) return `[${item.map(encode).join(",")}]`;
    if (item !== null && typeof item === "object")
      return `{${Object.keys(item)
        .sort(compareKeys)
        .map((key) => `${JSON.stringify(key)}:${encode(item[key])}`)
        .join(",")}}`;
    return JSON.stringify(item);
  };
  return encode(normalized);
}
