import {
  defaultStudioPreferences,
  parseStudioPreferences,
  applyStudioPreferences,
  studioPreferencesStorageKey,
} from "../studioPreferences";
import {
  scopedStorage,
  serverLocalStorage,
  observeStorageWrites,
} from "../servers/storage";
import {
  preferenceCacheKey,
  prunePreferenceFields,
  preferenceEvent,
  preferenceScope,
  storageFields,
  mergePreferenceFields,
  migratePreferenceFields,
  type PreferenceFields,
  type PreferenceScope,
} from "./uiPreferenceMerge";
const userStorage = () => globalThis.localStorage;
let applying = false;
const listeners = new Set<() => void>();
function cache(storage: Storage, scope: PreferenceScope): PreferenceFields {
  try {
    return prunePreferenceFields(
      JSON.parse(storage.getItem(`${preferenceCacheKey}:${scope}`) || "{}"),
    );
  } catch {
    return {};
  }
}
function persist(
  storage: Storage,
  scope: PreferenceScope,
  fields: PreferenceFields,
) {
  storage.setItem(
    `${preferenceCacheKey}:${scope}`,
    JSON.stringify(prunePreferenceFields(fields)),
  );
}
function writer() {
  let value = userStorage().getItem("studio-ui-preference-writer-v1");
  if (!value) {
    value = crypto.randomUUID();
    userStorage().setItem("studio-ui-preference-writer-v1", value);
  }
  return value;
}
export function capturePreferenceWrite(
  key: string,
  raw: string | null,
  previous: string | null,
  storage = serverLocalStorage,
) {
  if (applying) return;
  const scope = preferenceScope(key);
  if (!scope) return;
  const target = scope === "user" ? userStorage() : storage;
  const fields = cache(target, scope);
  const old = storageFields(key, previous);
  const next = storageFields(key, raw);
  const timestamp = Object.values(fields).reduce(
    (latest, field) => Math.max(latest, field.timestamp + 1),
    Date.now(),
  );
  let changed = false;
  for (const name of new Set([...Object.keys(old), ...Object.keys(next)])) {
    if (
      JSON.stringify(old[name]) === JSON.stringify(next[name]) ||
      JSON.stringify(fields[name]?.value) === JSON.stringify(next[name] ?? null)
    )
      continue;
    changed = true;
    fields[name] = { value: next[name] ?? null, timestamp, writer: writer() };
  }
  if (!changed) return;
  persist(target, scope, fields);
  listeners.forEach((listener) => listener());
  window.dispatchEvent(new CustomEvent(preferenceEvent, { detail: key }));
}
export function readPreferenceFields(
  scope: PreferenceScope,
  storage = serverLocalStorage,
) {
  return cache(scope === "user" ? userStorage() : storage, scope);
}
export function initializePreferenceMigration(
  scope: PreferenceScope,
  storage = serverLocalStorage,
) {
  const target = scope === "user" ? userStorage() : storage;
  const marker = `${preferenceCacheKey}:migrated:${scope}`;
  if (target.getItem(marker)) return;
  persist(
    target,
    scope,
    migratePreferenceFields(target, cache(target, scope), scope, writer()),
  );
  target.setItem(marker, "1");
}
export function receivePreferenceFields(
  scope: PreferenceScope,
  incoming: PreferenceFields,
  storage = serverLocalStorage,
  materialize = false,
) {
  const target = scope === "user" ? userStorage() : storage;
  const prior = cache(target, scope);
  const fields = mergePreferenceFields(prior, incoming);
  if (!materialize && JSON.stringify(prior) === JSON.stringify(fields)) return;
  applying = true;
  try {
    persist(target, scope, fields);
    const keys = new Map<string, Record<string, unknown>>();
    const scalars = new Map<string, unknown>();
    for (const [name, field] of Object.entries(fields)) {
      let parts: string[];
      try {
        parts = JSON.parse(name);
      } catch {
        continue;
      }
      const key = parts[0];
      if (preferenceScope(key) !== scope || key.startsWith("server-")) continue;
      if (parts.length === 1) scalars.set(key, field.value);
      else {
        if (!keys.has(key)) {
          try {
            keys.set(key, {
              ...(key === studioPreferencesStorageKey
                ? defaultStudioPreferences
                : {}),
              ...JSON.parse(target.getItem(key) || "{}"),
            });
          } catch {
            keys.set(
              key,
              key === studioPreferencesStorageKey
                ? { ...defaultStudioPreferences }
                : {},
            );
          }
        }
        const record = keys.get(key)!;
        if (field.value === null) delete record[parts[1]];
        else record[parts[1]] = field.value;
      }
    }
    for (const [key, value] of [...keys, ...scalars]) {
      const raw = JSON.stringify(value);
      if (target.getItem(key) === raw) continue;
      target.setItem(key, raw);
      window.dispatchEvent(new CustomEvent(preferenceEvent, { detail: key }));
      window.dispatchEvent(new StorageEvent("storage", { key, newValue: raw }));
      if (key === "codex-studio-preferences-v1") {
        if (value && typeof value === "object" && "theme" in value)
          userStorage().setItem(
            "mantine-color-scheme-value",
            String(value.theme),
          );
        window.dispatchEvent(new Event("studio-preferences-change"));
      }
    }
  } finally {
    applying = false;
  }
  listeners.forEach((listener) => listener());
}
export function watchPreferenceWrites(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
export function preferenceStorageForServer(id: string) {
  return scopedStorage(
    () => globalThis.localStorage,
    id === "local" ? "" : `:server:${id}:`,
    new Set(["codex-studio-preferences-v1", "mantine-color-scheme-value"]),
  );
}
observeStorageWrites(capturePreferenceWrite);

export function bootstrapPreferenceCache() {
  const element = document.getElementById("studio-ui-preferences");
  if (!element?.textContent) return;
  try {
    const seed = JSON.parse(element.textContent);
    const serverId =
      new URLSearchParams(location.search).get("studio-server") || "local";
    if (typeof seed.workspaceId === "string") {
      userStorage().setItem(
        `studio-server-workspace-id:${serverId}`,
        seed.workspaceId,
      );
      const alias =
        seed.user?.fields?.[
          JSON.stringify([`server-alias:${seed.workspaceId}`])
        ]?.value;
      if (
        serverId === "local" &&
        typeof alias === "string" &&
        !userStorage().getItem("studio-local-server-alias-v1")
      )
        userStorage().setItem("studio-local-server-alias-v1", alias);
    }
    for (const scope of ["user", "server"] as const) {
      const target = scope === "user" ? userStorage() : serverLocalStorage;
      const incoming = { ...seed[scope]?.fields } as PreferenceFields;
      // Existing local values must remain available for the one-time migration.
      for (const name of Object.keys(incoming)) {
        const [key] = JSON.parse(name);
        if (
          !target.getItem(`${preferenceCacheKey}:${scope}`) &&
          target.getItem(key) !== null
        )
          delete incoming[name];
      }
      receivePreferenceFields(scope, incoming);
    }
  } catch {
    /* An unavailable cache does not stop Studio. */
  }
}

/** Record the fields changed by this control, even when another field arrived before React rendered. */
export function writePreferenceEdit<T extends Record<string, unknown>>(
  key: string,
  previous: T,
  next: T,
  storage = serverLocalStorage,
): T {
  let latest: T;
  try {
    latest = { ...previous, ...JSON.parse(storage.getItem(key) || "{}") };
  } catch {
    latest = { ...previous };
  }
  for (const name of new Set([
    ...Object.keys(previous),
    ...Object.keys(next),
  ])) {
    if (JSON.stringify(previous[name]) !== JSON.stringify(next[name])) {
      if (name in next) latest[name as keyof T] = next[name as keyof T];
      else delete latest[name];
    }
  }
  applying = true;
  try {
    storage.setItem(key, JSON.stringify(latest));
  } finally {
    applying = false;
  }
  capturePreferenceWrite(
    key,
    JSON.stringify(next),
    JSON.stringify(previous),
    storage,
  );
  return latest;
}

export function applyCachedPreferenceAppearance() {
  try {
    const raw = userStorage().getItem(studioPreferencesStorageKey);
    if (!raw) return;
    const value = parseStudioPreferences(raw);
    applyStudioPreferences(value, document.documentElement);
    document.documentElement.dataset.mantineColorScheme =
      value.theme === "auto"
        ? window.matchMedia("(prefers-color-scheme: dark)").matches
          ? "dark"
          : "light"
        : value.theme;
    userStorage().setItem("mantine-color-scheme-value", value.theme);
  } catch {
    /* The existing preference loader reports invalid cache values. */
  }
}

/** Adopt the server clock correction only if this exact edit is still current. */
export function acknowledgePreferenceFields(
  scope: PreferenceScope,
  submitted: PreferenceFields,
  accepted: PreferenceFields,
  storage = serverLocalStorage,
) {
  const target = scope === "user" ? userStorage() : storage;
  const fields = cache(target, scope);
  for (const [name, sent] of Object.entries(submitted)) {
    if (JSON.stringify(fields[name]) !== JSON.stringify(sent)) continue;
    if (accepted[name]) fields[name] = accepted[name];
    else delete fields[name];
  }
  persist(target, scope, fields);
  receivePreferenceFields(scope, accepted, storage, true);
}
