export const LOCAL_ALIAS_KEY = "studio-local-server-alias-v1";
export const DEFAULT_LOCAL_SERVER_ALIAS = "LOC";
export const ALIASES_EVENT = "studio-server-aliases";

export function validateServerAlias(
  value: string,
  used: Iterable<string> = [],
) {
  if (!/^[A-Z]{1,3}$/.test(value))
    throw new Error("Use 1 to 3 uppercase letters.");
  if (new Set(used).has(value))
    throw new Error("This alias belongs to another server.");
  return value;
}

export function defaultServerAlias(
  server: { id: string; label: string; origin: string },
  used: Iterable<string> = [],
) {
  const taken = new Set(used);
  const host = new URL(server.origin).hostname.split(".")[0];
  const preferred =
    server.id === "local"
      ? DEFAULT_LOCAL_SERVER_ALIAS
      : (
          host.replace(/[^a-z]/gi, "").slice(0, 3) ||
          server.label.replace(/[^a-z]/gi, "").slice(0, 3) ||
          "SRV"
        ).toUpperCase();
  if (!taken.has(preferred)) return preferred;
  for (let i = 0; i < 26 ** 3; i++) {
    const candidate = [Math.floor(i / 676), Math.floor(i / 26) % 26, i % 26]
      .map((n) => String.fromCharCode(65 + n))
      .join("");
    if (!taken.has(candidate)) return candidate;
  }
  throw new Error("All server aliases are in use.");
}
