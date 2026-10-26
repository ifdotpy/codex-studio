import { useCallback, useMemo, useSyncExternalStore } from "react";
import { useServerStorage } from "../../servers/ServerServicesContext";
import { serverLocalStorage } from "../../servers/storage";
import {
  preferenceStorageForServer,
  readPreferenceFields,
  watchPreferenceWrites,
  writePreferenceValue,
} from "../../sync/uiPreferenceStore";
import {
  MODEL_WHITELIST_PREFERENCE_KEY,
  MODEL_WHITELIST_MAX_KEYS,
  MODEL_WHITELIST_KEY_MAX_CODEPOINTS,
  MODEL_WHITELIST_KEY_PATTERN,
} from "../../generated/sharedValues";
import {
  INHERITED_MODEL,
  modelForValue,
  type ModelOption,
} from "./modelOption";

/** null includes the entire catalog, including models added in the future. */
export type ModelWhitelist = readonly string[] | null;
const fieldName = JSON.stringify([MODEL_WHITELIST_PREFERENCE_KEY]);
const keyPattern = new RegExp(MODEL_WHITELIST_KEY_PATTERN);
export function validModelWhitelist(value: unknown): value is ModelWhitelist {
  return (
    value === null ||
    (Array.isArray(value) &&
      value.length <= MODEL_WHITELIST_MAX_KEYS &&
      new Set(value).size === value.length &&
      value.every(
        (key) =>
          typeof key === "string" &&
          [...key].length <= MODEL_WHITELIST_KEY_MAX_CODEPOINTS &&
          keyPattern.test(key),
      ))
  );
}
function storageFor(scope?: string | Storage) {
  if (typeof scope !== "string") return scope ?? serverLocalStorage;
  return scope === "default" || scope === "local"
    ? serverLocalStorage
    : preferenceStorageForServer(scope);
}
export function readModelWhitelist(scope?: string | Storage): ModelWhitelist {
  const field = readPreferenceFields("server", storageFor(scope))[fieldName];
  if (!field) return null;
  return validModelWhitelist(field.value) ? field.value : [];
}
export function modelWhitelistReadError(storage: Storage) {
  const field = readPreferenceFields("server", storage)[fieldName];
  return field && !validModelWhitelist(field.value)
    ? "Saved model selection is invalid. Choose models again to replace it."
    : "";
}
export const subscribeModelWhitelist = watchPreferenceWrites;
export function writeModelWhitelist(
  value: ModelWhitelist,
  scope?: string | Storage,
) {
  const unique = value === null ? null : [...new Set(value)];
  if (!validModelWhitelist(unique)) throw new Error("Invalid model selection.");
  writePreferenceValue(
    MODEL_WHITELIST_PREFERENCE_KEY,
    unique,
    storageFor(scope),
  );
}
export function useModelWhitelist() {
  const storage = useServerStorage();
  const getSnapshot = useCallback(
    () =>
      JSON.stringify(
        readPreferenceFields("server", storage)[fieldName] ?? null,
      ),
    [storage],
  );
  const serialized = useSyncExternalStore(
    subscribeModelWhitelist,
    getSnapshot,
    getSnapshot,
  );
  return useMemo(() => {
    const field = JSON.parse(serialized) as { value: unknown } | null;
    const value = field === null ? null : field.value;
    return validModelWhitelist(value) ? value : [];
  }, [serialized]);
}

/** Provider-qualified resolved identity keeps catalog aliases in one preference. */
export function modelWhitelistKey(model: {
  provider?: string | undefined;
  model?: string;
  value?: string;
  resolvedModel?: string;
}) {
  return `${model.provider || "codex"}:${model.resolvedModel || model.model || model.value}`;
}
export function isModelWhitelisted(
  model: Parameters<typeof modelWhitelistKey>[0],
  allowed: ModelWhitelist,
) {
  return allowed === null || allowed.includes(modelWhitelistKey(model));
}

/** Retain the current label without offering an excluded model as a new choice. */
export function whitelistedModelOptions(
  options: ModelOption[],
  value: string,
  allowed: ModelWhitelist,
  retainCurrent = true,
): ModelOption[] {
  const selected = modelForValue(options, value);
  return options.flatMap((option) => {
    if (option.value === INHERITED_MODEL || isModelWhitelisted(option, allowed))
      return [option];
    if (!retainCurrent || option !== selected) return [];
    return [
      {
        ...option,
        badges: [...new Set([...option.badges, "current"])],
        disabledReason:
          option.disabledReason ||
          "Hidden in Studio settings → Models. Existing chats keep their model.",
      },
    ];
  });
}
