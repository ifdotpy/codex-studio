import type { DraftVersion } from "../generated/rustOwned";

export function decodeDraftVersion(value: unknown): DraftVersion {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("A saved draft could not be read. Keep this chat open.");
  const row = value as Record<string, unknown>;
  const timestamp = (item: unknown) =>
    typeof item === "number" && Number.isSafeInteger(item) && item >= 0;
  if (
    typeof row.id !== "string" ||
    typeof row.session !== "string" ||
    typeof row.device !== "string" ||
    typeof row.text !== "string" ||
    !timestamp(row.updated) ||
    (row.alternatives !== undefined &&
      (!Array.isArray(row.alternatives) ||
        !row.alternatives.every(
          (item: unknown) => typeof item === "string",
        ))) ||
    (row.seen !== undefined &&
      (!row.seen ||
        typeof row.seen !== "object" ||
        Array.isArray(row.seen) ||
        !Object.values(row.seen).every(timestamp)))
  )
    throw new Error("A saved draft could not be read. Keep this chat open.");
  return value as DraftVersion;
}
