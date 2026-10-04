import type { JsonValue, Message } from "../types";
import { nativeErrorView } from "../nativeErrors";

type LimitNotice = { title: string; message: string } | null;
const cached = new WeakMap<
  Message,
  { text: string; status: unknown; error: unknown; notice: LimitNotice }
>();

export function toolLimitNotice(item: Message): LimitNotice {
  const prior = cached.get(item);
  if (
    prior &&
    prior.text === item.text &&
    prior.status === item.toolStatus &&
    prior.error === item.nativeError
  )
    return prior.notice;
  const notice = readToolLimitNotice(item);
  cached.set(item, {
    text: item.text,
    status: item.toolStatus,
    error: item.nativeError,
    notice,
  });
  return notice;
}

// Received worker failures use result; native tool failures use error/contentItems.
// Inspect only failed records. A successful tool can quote an unrelated error.
function readToolLimitNotice(
  item: Message,
): { title: string; message: string } | null {
  let payload: unknown;
  try {
    payload = JSON.parse(item.text);
  } catch {
    return null;
  }
  const record = objectValue(payload);
  if (!record) return null;
  if (
    item.toolStatus !== "failed" &&
    record.status !== "failed" &&
    record.success !== false
  )
    return null;
  const result = objectValue(record.result);
  const worker = typeof record.agent_id === "string" && "result" in record;
  const textEntries = (value: JsonValue | undefined): string[] =>
    Array.isArray(value)
      ? value.flatMap((entry) => {
          const entryRecord = objectValue(entry);
          return typeof entryRecord?.text === "string"
            ? [entryRecord.text]
            : [];
        })
      : [];
  const candidates = [
    record.error,
    record.result,
    item.nativeError,
    ...textEntries(record.contentItems),
    ...textEntries(result?.contentItems),
    ...textEntries(result?.content),
  ];
  for (const candidate of candidates) {
    if (candidate == null) continue;
    const error = nativeErrorView(candidate);
    if (
      !["usageLimitExceeded", "rateLimitExceeded"].includes(error.kind) &&
      !/^(?:You've hit your usage limit\b|Usage limit (?:reached|exceeded)\b)/i.test(
        error.message,
      )
    )
      continue;
    // Keep the server's displayed date as text; its timezone can be unspecified.
    const retry = error.message.match(/\btry again at\s+(.+?)\.?\s*$/i)?.[1];
    const name = worker && typeof record.name === "string" ? record.name : "";
    const title =
      error.kind === "rateLimitExceeded"
        ? "Rate limit reached"
        : "Usage limit reached";
    return {
      title: worker ? `Worker stopped: ${title.toLowerCase()}` : title,
      message: [
        name,
        retry
          ? `Try again at ${retry}.`
          : worker
            ? "Check this worker's account limits before retrying."
            : "Check the account limits before retrying.",
      ]
        .filter(Boolean)
        .join(" · "),
    };
  }
  return null;
}

function objectValue(value: unknown): Record<string, JsonValue> | null {
  return isJsonObject(value) ? value : null;
}

function isJsonObject(value: unknown): value is Record<string, JsonValue> {
  return (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.values(value).every(isJsonValue)
  );
}

function isJsonValue(value: unknown): value is JsonValue {
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "boolean" ||
    (typeof value === "number" && Number.isFinite(value))
  )
    return true;
  if (Array.isArray(value)) return value.every(isJsonValue);
  return isJsonObject(value);
}
