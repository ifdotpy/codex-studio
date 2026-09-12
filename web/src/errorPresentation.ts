// Server diagnostics are data, not React children. Keep their original fields
// available separately from the short message used in labels and notifications.
export function errorDetails(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  if (typeof value !== "object") return String(value);
  try {
    const seen = new WeakSet<object>();
    return (
      JSON.stringify(
        value,
        (_key, item) => {
          if (typeof item === "bigint") return String(item);
          if (!item || typeof item !== "object") return item;
          if (seen.has(item)) return "[Repeated reference]";
          seen.add(item);
          return item instanceof Error
            ? {
                ...item,
                name: item.name,
                message: item.message,
                stack: item.stack,
                cause: item.cause,
              }
            : item;
        },
        2,
      ) || ""
    );
  } catch {
    return "Error details could not be displayed.";
  }
}

export function displayError(value: unknown): string {
  const visited = new Set<unknown>();
  function message(current: unknown, depth: number): string {
    if (current == null) return "";
    if (typeof current === "string") return current;
    if (typeof current !== "object") return String(current);
    if (depth >= 8 || visited.has(current)) return "";
    visited.add(current);
    const data = current as Record<string, unknown>;
    for (const child of [data.message, data.error, data.reason]) {
      const text = message(child, depth + 1);
      if (text.trim()) return text;
    }
    return "";
  }
  try {
    return message(value, 0) || errorDetails(value);
  } catch {
    return errorDetails(value);
  }
}
