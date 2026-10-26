import contract from "../../../../../contracts/preference-storage.json";

/** Exact decimal revisions are SQLite transaction versions, not timestamps. */
export function readNativeRevision(value: unknown): string {
  const maximum = contract.revision.maximum;
  if (
    typeof value !== "string" ||
    !/^(0|[1-9][0-9]*)$/.test(value) ||
    value.length > maximum.length ||
    (value.length === maximum.length && value > maximum)
  )
    throw new Error("The native storage revision is invalid.");
  return value;
}
export function compareNativeRevisions(
  left: string,
  right: string,
): -1 | 0 | 1 {
  readNativeRevision(left);
  readNativeRevision(right);
  return left === right
    ? 0
    : left.length === right.length
      ? left < right
        ? -1
        : 1
      : left.length < right.length
        ? -1
        : 1;
}
