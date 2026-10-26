export const PREFERENCE_JOURNAL = "studio-native-preferences-journal-v1";
// This bounds one legacy preference queue, not the native store's capacity.
const MAX_PENDING_PREFERENCES = 10_000;
export type PendingPreference = {
  id: string;
  method: string;
  params: Record<string, unknown>;
};

/** Validate saved values without browser IO or a new request identity. */
export function readPendingLegacyPreferences(
  storage: Pick<Storage, "getItem">,
): PendingPreference[] {
  const writes: unknown = JSON.parse(
    storage.getItem(PREFERENCE_JOURNAL) || "[]",
  );
  if (
    !Array.isArray(writes) ||
    writes.length > MAX_PENDING_PREFERENCES ||
    writes.some(
      (write) =>
        !write ||
        typeof write.id !== "string" ||
        !write.id ||
        write.id.length > 128 ||
        ![
          "preferences.raw_set",
          "preferences.raw_remove",
          "preferences.import",
        ].includes(write.method) ||
        !write.params ||
        typeof write.params !== "object" ||
        Array.isArray(write.params),
    )
  )
    throw new Error(
      "The legacy preference journal is invalid. The source was preserved.",
    );
  return writes as PendingPreference[];
}
