import type { Message } from "../types";

export interface HistoryGroup {
  id: string;
  items: Message[];
  outcome?: string;
  result?: Message;
}

// A terminal record is required. Silence, a completed tool, or a final-looking
// paragraph alone does not prove that its turn ended.
export function historyGroups(
  items: Message[],
  currentTurn?: string,
): HistoryGroup[] {
  const groups: HistoryGroup[] = [];
  for (const item of items) {
    const last = groups.at(-1);
    if (
      last &&
      item.role !== "user" &&
      !item.pending &&
      last.items[0].role !== "user" &&
      last.items[0].turnId === item.turnId
    )
      last.items.push(item);
    else groups.push({ id: item.id, items: [item] });
  }
  for (const group of groups) {
    const first = group.items[0];
    if (
      !first.turnId ||
      first.turnId === currentTurn ||
      first.role === "user" ||
      group.items.some(
        (item) =>
          item.streaming || item.pending || item.toolStatus === "running",
      )
    )
      continue;
    const outcomes = group.items.map((item) => item.turnStatus).filter(Boolean);
    if (
      !outcomes.length ||
      outcomes.some(
        (status) =>
          !["completed", "failed", "interrupted", "ended"].includes(status),
      )
    )
      continue;
    group.outcome = outcomes.includes("failed")
      ? "failed"
      : outcomes.includes("interrupted")
        ? "interrupted"
        : outcomes.includes("ended")
          ? "ended"
          : "completed";
    const answers = group.items.filter(
      (item) => item.role === "assistant" && item.text.trim(),
    );
    group.result =
      answers.filter((item) => item.phase === "final_answer").at(-1) ||
      answers.at(-1);
  }
  return groups;
}

export function resultExcerpt(text: string) {
  const paragraph = text.trim().split(/\n\s*\n/)[0] || "";
  const plain = paragraph
    .replace(/!?(?:\[([^\]]+)\])\([^)]*\)/g, "$1")
    .replace(/^[#>*\s-]+/, "")
    .replace(/[`*_]/g, "")
    .replace(/\s+/g, " ")
    .trim();
  return plain.length > 240 ? plain.slice(0, 240).trimEnd() + "…" : plain;
}
