import type {
  MantineColorScheme,
  MantineColorSchemeManager,
} from "@mantine/core";

/** Native preference key for the user's visible color scheme. Mantine parity is tested. */
export const NATIVE_COLOR_SCHEME_KEY = "mantine-color-scheme-value";
interface ColorSchemePreferences {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
  subscribe(listener: (key: string | null) => void): () => void;
}
interface Installation {
  owner: ColorSchemePreferences;
  subscriptions: Set<() => void>;
}
let installed: Installation | undefined;

/** The native mount owns this lifetime and installs its prepared preference cache. */
export function installNativeColorSchemePreferences(
  owner: ColorSchemePreferences,
) {
  if (installed)
    throw new Error(
      "The native color scheme preference owner is already installed.",
    );
  const installation: Installation = { owner, subscriptions: new Set() };
  installed = installation;
  return () => {
    if (installed !== installation) return;
    for (const unsubscribe of installation.subscriptions) unsubscribe();
    installed = undefined;
  };
}
function isColorScheme(value: string | null): value is MantineColorScheme {
  return value === "auto" || value === "light" || value === "dark";
}

/** Exact replacement for Mantine's default manager, backed by the native UI cache. */
export function localStorageColorSchemeManager({
  key = NATIVE_COLOR_SCHEME_KEY,
}: { key?: string } = {}): MantineColorSchemeManager {
  const installation = installed;
  if (!installation)
    throw new Error("The native color scheme preference owner is unavailable.");
  function requireOwner() {
    if (!installation || installed !== installation)
      throw new Error(
        "The native color scheme preference owner is unavailable.",
      );
    return installation.owner;
  }
  requireOwner();
  let unsubscribe: (() => void) | undefined;
  return {
    get(defaultValue) {
      const value = requireOwner().getItem(key);
      return isColorScheme(value) ? value : defaultValue;
    },
    set(value) {
      if (!isColorScheme(value))
        throw new Error("The color scheme is invalid.");
      requireOwner().setItem(key, value);
    },
    subscribe(onUpdate) {
      unsubscribe?.();
      const release = requireOwner().subscribe((changed) => {
        if (changed !== null && changed !== key) return;
        const value = requireOwner().getItem(key);
        if (isColorScheme(value)) onUpdate(value);
      });
      const close = () => {
        release();
        installation.subscriptions.delete(close);
      };
      unsubscribe = close;
      installation.subscriptions.add(close);
    },
    unsubscribe() {
      unsubscribe?.();
      unsubscribe = undefined;
    },
    clear() {
      requireOwner().removeItem(key);
    },
  };
}
