import { useModelCatalogReader } from "../agents/WorkerModelPicker";
import { useCallback, useSyncExternalStore } from "react";
import {
  INHERITED_MODEL,
  modelForValue,
  type ModelOption,
} from "./modelOption";

/** null includes the entire catalog, including models added in the future. */
export type ModelWhitelist = readonly string[] | null;
// Temporary in-memory storage for the experimental redesign. Persistence is
// pending the owner's contract decision; replace only this read/write/subscribe
// boundary when the approved preference backend is available.
const whitelists = new Map<string, ModelWhitelist>();
const listeners = new Set<() => void>();
export const readModelWhitelist = (scope = "default") =>
  whitelists.get(scope) ?? null;
export function subscribeModelWhitelist(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
export function writeModelWhitelist(value: ModelWhitelist, scope = "default") {
  if (value === null) whitelists.delete(scope);
  else whitelists.set(scope, [...new Set(value)]);
  listeners.forEach((listener) => listener());
}
export function useModelWhitelist() {
  const { scope } = useModelCatalogReader();
  return useSyncExternalStore(
    subscribeModelWhitelist,
    useCallback(() => readModelWhitelist(scope), [scope]),
  );
}

/** Provider-qualified resolved identity keeps catalog aliases in one preference. */
export function modelWhitelistKey(model: {
  provider?: string;
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
