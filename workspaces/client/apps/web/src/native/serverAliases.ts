import { useEffect, useState } from "react";
import { nativeRegistry } from "./registry";
import { validateServerAlias } from "../servers/serverAliasModel";
export * from "../servers/serverAliasModel";

const listeners = new Set<() => void>();
let frameAliases: Record<string, string> | undefined;
/** Cross-screen aliases are display state only; they authorize no native request. */
export function publishServerAliases(aliases: Record<string, string>) {
  const used = new Set<string>();
  for (const alias of Object.values(aliases)) {
    validateServerAlias(alias, used);
    used.add(alias);
  }
  frameAliases = { ...aliases };
  for (const listener of listeners) listener();
}
export function useServerAliases(read: () => Record<string, string>) {
  const [aliases, setAliases] = useState(() => frameAliases || read());
  useEffect(() => {
    const changed = () => setAliases(frameAliases || read());
    const stop = nativeRegistry().subscribe(changed);
    listeners.add(changed);
    return () => {
      stop();
      listeners.delete(changed);
    };
  }, [read]);
  return aliases;
}
