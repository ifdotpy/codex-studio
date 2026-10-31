import { setPreferenceWriteStatus } from "./uiPreferenceStatus";
import { randomUUID } from "../uuid";
import {
  defaultStudioPreferences,
  normalizeStudioPreferences,
  studioPreferencesStorageKey,
} from "../studioPreferences";
import {
  preferenceStorageForServer as scopedPreferenceStorage,
  serverLocalStorage,
  observeStorageWrites,
  preferenceMemory,
} from "../servers/storage";
import {
  preferenceCacheKey,
  prunePreferenceFields,
  preferenceEvent,
  preferenceScope,
  storageFields,
  mergePreferenceFields,
  type PreferenceFields,
  type PreferenceScope,
} from "./uiPreferenceMerge";
import { RETIRED_MODEL_WHITELIST_KEY } from "../generated/sharedValues.ts";
const userStorage = () => preferenceMemory;
const retiredModelWhitelistField = JSON.stringify([
  RETIRED_MODEL_WHITELIST_KEY,
]);
let applying = false;
const listeners = new Set<() => void>();
function cache(storage: Storage, scope: PreferenceScope): PreferenceFields {
  const key = `${preferenceCacheKey}:${scope}`;
  try {
    const parsed: unknown = JSON.parse(storage.getItem(key) || "{}");
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed))
      return {};
    const entries = Object.entries(parsed);
    const valid = Object.fromEntries(
      entries.filter(
        ([, field]) =>
          field &&
          typeof field === "object" &&
          !Array.isArray(field) &&
          "value" in field &&
          "timestamp" in field &&
          typeof field.timestamp === "number" &&
          Number.isSafeInteger(field.timestamp) &&
          field.timestamp >= 0 &&
          "writer" in field &&
          typeof field.writer === "string" &&
          field.writer.length > 0,
      ),
    ) as PreferenceFields;
    const fields = prunePreferenceFields(valid);
    const validFields = Object.fromEntries(
      Object.entries(fields).filter(([name, field]) => {
        // The server rejects every push that still carries the retired field.
        if (scope !== "user") return name !== retiredModelWhitelistField;
        let parts: unknown;
        try {
          parts = JSON.parse(name);
        } catch {
          return true;
        }
        if (!Array.isArray(parts) || parts[0] !== studioPreferencesStorageKey)
          return true;
        const fieldName = parts[1];
        if (
          parts.length !== 2 ||
          typeof fieldName !== "string" ||
          !Object.hasOwn(defaultStudioPreferences, fieldName)
        )
          return false;
        try {
          normalizeStudioPreferences({
            ...defaultStudioPreferences,
            [fieldName]: field.value,
          });
          return true;
        } catch {
          return false;
        }
      }),
    );
    if (Object.keys(validFields).length !== entries.length)
      storage.setItem(key, JSON.stringify(validFields));
    return validFields;
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
    value = randomUUID();
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
  setPreferenceWriteStatus(storage, scope, { state: "saving" });
  listeners.forEach((listener) => listener());
  window.dispatchEvent(new CustomEvent(preferenceEvent, { detail: key }));
}
export function readPreferenceFields(
  scope: PreferenceScope,
  storage = serverLocalStorage,
) {
  return cache(scope === "user" ? userStorage() : storage, scope);
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
      let parts: [string] | [string, string];
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
  return scopedPreferenceStorage(id);
}
observeStorageWrites(capturePreferenceWrite);

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

/** Write one scalar field, including an explicit null, without replacing siblings. */
export function writePreferenceValue(
  key: string,
  value: PreferenceFields[string]["value"],
  storage = serverLocalStorage,
) {
  const scope = preferenceScope(key);
  if (!scope) throw new Error("Unsupported preference");
  const target = scope === "user" ? userStorage() : storage;
  const fields = cache(target, scope);
  const name = JSON.stringify([key]);
  if (
    fields[name] &&
    JSON.stringify(fields[name].value) === JSON.stringify(value)
  )
    return;
  const timestamp = Object.values(fields).reduce(
    (latest, field) => Math.max(latest, field.timestamp + 1),
    Date.now(),
  );
  fields[name] = { value, timestamp, writer: writer() };
  persist(target, scope, fields);
  setPreferenceWriteStatus(storage, scope, { state: "saving" });
  listeners.forEach((listener) => listener());
  window.dispatchEvent(new CustomEvent(preferenceEvent, { detail: key }));
}
