import { useEffect, useState } from "react";

export const LOCAL_ALIAS_KEY = "studio-local-server-alias-v1";
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
  const name = `${server.label} ${host}`.toLowerCase();
  const preferred =
    server.id === "local"
      ? "MAC"
      : name.includes("igor-mbp")
        ? "MBP"
        : name.includes("kukuka-win")
          ? new URL(server.origin).port === "8443"
            ? "WIN"
            : "WSL"
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

let frameAliases: Record<string, string> | null = null;
export function publishServerAliases(aliases: Record<string, string>) {
  frameAliases = aliases;
  window.dispatchEvent(new Event(ALIASES_EVENT));
}
export function useServerAliases(read: () => Record<string, string>) {
  const [aliases, setAliases] = useState(() => frameAliases || read());
  useEffect(() => {
    const update = () => setAliases(frameAliases || read());
    window.addEventListener(ALIASES_EVENT, update);
    window.addEventListener("studio-server-registry", update);
    window.addEventListener("storage", update);
    return () => {
      window.removeEventListener(ALIASES_EVENT, update);
      window.removeEventListener("studio-server-registry", update);
      window.removeEventListener("storage", update);
    };
  }, []);
  return aliases;
}
