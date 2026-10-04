function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function objectValue(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

export function nativeStatusMessage(status: unknown): string | undefined {
  const fields = objectValue(status);
  if (!fields) return undefined;

  const error = fields.error;
  if (typeof error === "string" && error.length > 0) return error;
  const errorMessage = objectValue(error)?.message;
  return (
    nonEmptyString(errorMessage) || nonEmptyString(fields.message) || undefined
  );
}
