import type { Message } from "../types";
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
  let payload: any;
  try {
    payload = JSON.parse(item.text);
  } catch {
    return null;
  }
  if (!payload || typeof payload !== "object") return null;
  if (
    item.toolStatus !== "failed" &&
    payload.status !== "failed" &&
    payload.success !== false
  )
    return null;
  const worker = typeof payload.agent_id === "string" && "result" in payload;
  const candidates = [
    payload.error,
    payload.result,
    item.nativeError,
    ...[
      payload.contentItems,
      payload.result?.contentItems,
      payload.result?.content,
    ].flatMap((entries) =>
      Array.isArray(entries) ? entries.map((entry: any) => entry?.text) : [],
    ),
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
    const name = worker && typeof payload.name === "string" ? payload.name : "";
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
