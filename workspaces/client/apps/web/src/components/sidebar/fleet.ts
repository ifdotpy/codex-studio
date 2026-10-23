import type { Agent } from "../../types";
import type {
  ChatIndicator,
  ChatIndicatorKind,
} from "../chat-status/chatStatusModel";

/** Presentation order for the operator lane; operational state belongs to chatStatusModel. */
const attentionOrder: readonly ChatIndicatorKind[] = ["answer", "error"];
export function needsYou(
  chats: readonly Agent[],
  indicators: ReadonlyMap<string, ChatIndicator>,
) {
  return chats
    .filter((chat) =>
      attentionOrder.includes(indicators.get(chat.id)?.kind ?? "none"),
    )
    .sort(
      (a, b) =>
        attentionOrder.indexOf(indicators.get(a.id)!.kind) -
        attentionOrder.indexOf(indicators.get(b.id)!.kind),
    );
}

export const aggregateStates = [
  "working",
  "answer",
  "error",
] as const satisfies readonly ChatIndicatorKind[];
export function projectStateCounts(
  chats: readonly Agent[],
  indicators: ReadonlyMap<string, ChatIndicator>,
) {
  const counts = { working: 0, answer: 0, error: 0 };
  for (const chat of chats) {
    const kind = indicators.get(chat.id)?.kind;
    if (kind === "working" || kind === "answer" || kind === "error")
      counts[kind]++;
  }
  return counts;
}
export function projectStateLabel(
  counts: ReturnType<typeof projectStateCounts>,
) {
  return [
    counts.working && `${counts.working} working`,
    counts.answer && `${counts.answer} needs you`,
    counts.error && `${counts.error} failed`,
  ]
    .filter(Boolean)
    .join(", ");
}

/** Durations in seconds for compact, tabular fleet timestamps. */
const minuteSeconds = 60;
const hourSeconds = 60 * minuteSeconds;
const daySeconds = 24 * hourSeconds;
export function relativeChatTime(timestamp: number, now = Date.now() / 1000) {
  const elapsed = Math.max(0, now - timestamp);
  if (elapsed < minuteSeconds) return "now";
  if (elapsed < hourSeconds) return `${Math.floor(elapsed / minuteSeconds)}m`;
  if (elapsed < daySeconds) return `${Math.floor(elapsed / hourSeconds)}h`;
  return `${Math.floor(elapsed / daySeconds)}d`;
}
