import type { components } from "../generated/api";
export type PreferenceField = components["schemas"]["PreferenceField"];
export type PreferenceFields = Record<string, PreferenceField>;
export type PreferenceScope = "user" | "server";
export const preferenceCacheKey = "studio-ui-preference-fields-v1";
export const preferenceEvent = "studio-ui-preference-change";
export function preferenceScope(key: string): PreferenceScope | null {
  if (
    key === "codex-studio-preferences-v1" ||
    key.startsWith("server-") ||
    key.startsWith("studio-logical-project-") ||
    key === "studio-server-project-compact-v1"
  )
    return "user";
  if (
    key.startsWith("codex-project-tree:") ||
    key.startsWith("codex-project-compact:") ||
    key.startsWith("codex-progress-hidden:") ||
    key.startsWith("studio-prompt-bookmarks:") ||
    key === "codex-worker-disclosures" ||
    (key.startsWith("studio-turns:") && key.endsWith(":tools-v3"))
  )
    return "server";
  return null;
}
export function canonicalPreferenceJson(value: unknown): string {
  if (Array.isArray(value))
    return `[${value.map(canonicalPreferenceJson).join(",")}]`;
  if (value && typeof value === "object")
    return `{${Object.entries(value)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(
        ([key, entry]) =>
          `${JSON.stringify(key)}:${canonicalPreferenceJson(entry)}`,
      )
      .join(",")}}`;
  return JSON.stringify(value);
}
export function mergePreferenceFields(
  prior: PreferenceFields,
  incoming: PreferenceFields,
): PreferenceFields {
  const result = { ...prior };
  for (const [key, field] of Object.entries(incoming)) {
    if (
      !field ||
      !Number.isSafeInteger(field.timestamp) ||
      field.timestamp < 0 ||
      typeof field.writer !== "string" ||
      !field.writer
    )
      continue;
    const old = result[key];
    if (
      !old ||
      field.timestamp > old.timestamp ||
      (field.timestamp === old.timestamp &&
        (field.writer > old.writer ||
          (field.writer === old.writer &&
            canonicalPreferenceJson(field.value) >
              canonicalPreferenceJson(old.value))))
    )
      result[key] = field;
  }
  return result;
}
export function storageFields(
  key: string,
  raw: string | null,
): Record<string, PreferenceField["value"]> {
  let value: PreferenceField["value"];
  try {
    value = raw === null ? null : JSON.parse(raw);
  } catch {
    return {};
  }
  if (value && typeof value === "object" && !Array.isArray(value))
    return Object.fromEntries(
      Object.entries(value).map(([entry, val]) => [
        JSON.stringify([key, entry]),
        val,
      ]),
    );
  if (value === null) return {};
  return { [JSON.stringify([key])]: value };
}
export function migratePreferenceFields(
  storage: Storage,
  prior: PreferenceFields,
  scope: PreferenceScope,
  writer: string,
): PreferenceFields {
  const result = { ...prior };
  for (let i = 0; i < storage.length; i++) {
    const key = storage.key(i)!;
    if (preferenceScope(key) !== scope) continue;
    for (const [name, value] of Object.entries(
      storageFields(key, storage.getItem(key)),
    )) {
      // A legacy choice has no edit time. It fills absent fields and cannot replace server intent.
      if (!result[name]) result[name] = { value, timestamp: 0, writer };
    }
  }
  return result;
}
