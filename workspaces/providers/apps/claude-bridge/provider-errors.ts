import {
  KNOWN_CODEX_ERROR_KINDS,
  type KnownCodexErrorKind,
} from "./provider-errors.generated.ts";

/** A classification projection only: callers retain the original error record. */
export function rawCodexErrorKind(info: unknown): string | undefined {
  if (typeof info === "string") return info;
  if (!info || typeof info !== "object" || Array.isArray(info))
    return undefined;
  const keys = Object.keys(info);
  return keys.length === 1 ? keys[0] : undefined;
}

export function isKnownCodexErrorKind(
  value: unknown,
): value is KnownCodexErrorKind {
  return KNOWN_CODEX_ERROR_KINDS.some((kind) => kind === value);
}

export function knownCodexErrorKind(
  info: unknown,
): KnownCodexErrorKind | undefined {
  const tag = rawCodexErrorKind(info);
  return isKnownCodexErrorKind(tag) ? tag : undefined;
}
