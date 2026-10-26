import contract from "../../../../../contracts/preference-storage.json";
import type { PreferenceWriteStatus } from "../../sync/uiPreferenceStatus";

type NativeRemoteStatus =
  | Exclude<PreferenceWriteStatus, { state: "error" }>
  | {
      state: "error";
      error: string;
      uncertain: boolean;
      requestId: string;
    };
export type NativePreferenceSync = Record<
  keyof typeof contract.scopes,
  NativeRemoteStatus
>;
function record(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
type Schema = {
  properties: Record<
    string,
    { const?: unknown; type?: string; minLength?: number } | undefined
  >;
  required: string[];
  additionalProperties: boolean;
};
function matches(value: Record<string, unknown>, schema: Schema) {
  if (schema.required.some((key) => !Object.hasOwn(value, key))) return false;
  if (
    !schema.additionalProperties &&
    Object.keys(value).some((key) => !Object.hasOwn(schema.properties, key))
  )
    return false;
  return Object.entries(schema.properties).every(([key, rule]) => {
    if (!rule) return false;
    const item = value[key];
    if (Object.hasOwn(rule, "const") && item !== rule.const) return false;
    if (rule.type && typeof item !== rule.type) return false;
    return (
      rule.minLength === undefined ||
      (typeof item === "string" && item.length >= rule.minLength)
    );
  });
}
/** Validate host remote receipts against the shared schema before showing them. */
export function readNativePreferenceSync(value: unknown): NativePreferenceSync {
  const scopes = Object.keys(contract.scopes);
  if (
    !record(value) ||
    Object.keys(value).length !== scopes.length ||
    scopes.some((scope) => {
      const status = value[scope];
      return (
        !record(status) ||
        !contract.remoteStatusSchema.oneOf.some((schema) =>
          matches(status, schema),
        )
      );
    })
  )
    throw new Error("The native preference sync status is invalid.");
  return value as NativePreferenceSync;
}
