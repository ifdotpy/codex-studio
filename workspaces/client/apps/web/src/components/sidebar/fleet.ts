import type { Agent } from "../../types";
import {
  CHAT_INDICATOR_URGENCY,
  type ChatIndicator,
  type ChatIndicatorKind,
} from "../chat-status/chatStatusModel";

export function needsYou(
  chats: readonly Agent[],
  indicators: ReadonlyMap<string, ChatIndicator>,
) {
  return chats
    .filter((chat) => {
      const kind = indicators.get(chat.id)?.kind;
      return kind === "answer" || kind === "error";
    })
    .sort(
      (a, b) =>
        CHAT_INDICATOR_URGENCY.indexOf(indicators.get(a.id)!.kind) -
        CHAT_INDICATOR_URGENCY.indexOf(indicators.get(b.id)!.kind),
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

/** Refresh all fleet timestamps together, in milliseconds. */
export const FLEET_CLOCK_INTERVAL_MS = 60_000;

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
