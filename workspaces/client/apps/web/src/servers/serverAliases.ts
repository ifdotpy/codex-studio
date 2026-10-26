import { useEffect, useState } from "react";
import { ALIASES_EVENT } from "./serverAliasModel";
export * from "./serverAliasModel";

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
