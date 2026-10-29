import { ModelChoice, type components } from "../../generated/api";
import { useCallback, useMemo, useSyncExternalStore } from "react";
import { useServerStorage } from "../../servers/ServerServicesContext";
import { serverLocalStorage } from "../../servers/storage";
import {
  preferenceStorageForServer,
  readPreferenceFields,
  watchPreferenceWrites,
  writePreferenceValue,
} from "../../sync/uiPreferenceStore";
import { MODEL_CHOICES_PREFERENCE_KEY } from "../../generated/sharedValues";
import {
  INHERITED_MODEL,
  modelForValue,
  type ModelOption,
} from "./modelOption";

/** The server validates every stored value against this generated schema. */
export type ModelChoices = components["schemas"]["ModelChoices"];
const fieldName = JSON.stringify([MODEL_CHOICES_PREFERENCE_KEY]);
function choicesOf(field: { value: unknown } | null | undefined) {
  return (field?.value ?? {}) as ModelChoices;
}
function storageFor(scope?: string | Storage) {
  if (typeof scope !== "string") return scope ?? serverLocalStorage;
  return scope === "default" || scope === "local"
    ? serverLocalStorage
    : preferenceStorageForServer(scope);
}
export function readModelChoices(scope?: string | Storage): ModelChoices {
  return choicesOf(
    readPreferenceFields("server", storageFor(scope))[fieldName],
  );
}
export const subscribeModelChoices = watchPreferenceWrites;
export function writeModelChoices(
  value: ModelChoices,
  scope?: string | Storage,
) {
  writePreferenceValue(MODEL_CHOICES_PREFERENCE_KEY, value, storageFor(scope));
}
export function useModelChoices() {
  const storage = useServerStorage();
  const getSnapshot = useCallback(
    () =>
      JSON.stringify(
        readPreferenceFields("server", storage)[fieldName] ?? null,
      ),
    [storage],
  );
  const serialized = useSyncExternalStore(
    subscribeModelChoices,
    getSnapshot,
    getSnapshot,
  );
  return useMemo(() => choicesOf(JSON.parse(serialized)), [serialized]);
}

/** Provider-qualified resolved identity keeps catalog aliases in one preference. */
export function modelChoiceKey(model: {
  provider?: string | undefined;
  model?: string;
  value?: string;
  resolvedModel?: components["schemas"]["ProviderModel"]["resolvedModel"];
}) {
  return `${model.provider || ""}:${model.resolvedModel || model.model || model.value}`;
}
export function modelChoice(
  model: Parameters<typeof modelChoiceKey>[0],
  choices: ModelChoices,
): ModelChoice {
  return choices[modelChoiceKey(model)] ?? ModelChoice.untouched;
}
export function isModelEnabled(
  model: Parameters<typeof modelChoiceKey>[0],
  choices: ModelChoices,
) {
  const choice = modelChoice(model, choices);
  return choice === ModelChoice.whitelisted || choice === ModelChoice.untouched;
}

/** Retain the current label without offering an excluded model as a new choice. */
export function enabledModelOptions(
  options: ModelOption[],
  value: string,
  choices: ModelChoices,
  retainCurrent = true,
): ModelOption[] {
  const selected = modelForValue(options, value);
  return options.flatMap((option) => {
    if (option.value === INHERITED_MODEL || isModelEnabled(option, choices))
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
