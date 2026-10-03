import type { Message } from "../types";

// Native turns can finish without a reply, for example after a worker event.
// Retain their records for turn status, but do not draw an empty message.
export function isEmptyAssistantMessage(item: Message): boolean {
  return (
    item.role === "assistant" &&
    !item.text.trim() &&
    !item.assets?.length &&
    !item.nativeNotice &&
    !item.nativeError &&
    !item.truncated
  );
}

export interface HistoryGroup {
  id: string;
  items: Message[];
  outcome?: string;
  result?: Message;
  /** Native turn groups represented by this renderer-only presentation group. */
  turns?: HistoryGroup[];
}

function activityRow(item: Message): boolean {
  if (item.pending || item.nativeNotice || item.nativeError) return false;
  if (item.role === "reasoning") return true;
  if (["tool", "output"].includes(item.role)) {
    if (item.title === "fileChange") return false;
    try {
      return JSON.parse(item.text)?.type !== "fileChange";
    } catch {
      return true;
    }
  }
  return (
    item.role === "assistant" &&
    !item.streaming &&
    isEmptyAssistantMessage(item)
  );
}

function activityEdge(group: HistoryGroup, fromEnd: boolean): boolean {
  const rows = fromEnd ? [...group.items].reverse() : group.items;
  for (const item of rows) {
    if (isEmptyAssistantMessage(item)) continue;
    return activityRow(item);
  }
  return false;
}

/** Join only uninterrupted tool/reasoning presentation runs. Native turn
 * groups remain attached so outcome and error attribution never crosses turns. */
export function historyPresentationGroups(
  groups: HistoryGroup[],
): HistoryGroup[] {
  const result: HistoryGroup[] = [];
  for (const group of groups) {
    const previous = result.at(-1);
    const priorTurns = previous?.turns || (previous ? [previous] : []);
    const priorNativeTurn = priorTurns.at(-1);
    const joinableOutcome =
      !group.outcome ||
      ["completed", "failed", "interrupted", "ended"].includes(group.outcome);
    const canJoin =
      previous &&
      priorNativeTurn?.outcome === "completed" &&
      joinableOutcome &&
      activityEdge(previous, true) &&
      activityEdge(group, false);
    if (!canJoin) {
      result.push({ ...group, turns: [group] });
      continue;
    }
    const turns = [...priorTurns, group];
    const items = turns.flatMap((turn) => turn.items);
    result[result.length - 1] = {
      id: items[0].id,
      items,
      outcome: turns.at(-1)?.outcome,
      result: turns.at(-1)?.result,
      turns,
    };
  }
  return result;
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
    // An explicit final answer can stream before the terminal event arrives.
    group.result = group.items
      .filter(
        (item) =>
          item.role === "assistant" &&
          item.phase === "final_answer" &&
          !isEmptyAssistantMessage(item),
      )
      .at(-1);
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
      (item) => item.role === "assistant" && !isEmptyAssistantMessage(item),
    );
    group.result =
      answers.filter((item) => item.phase === "final_answer").at(-1) ||
      answers.at(-1);
  }
  return groups;
}

/** Reuse complete groups before the first changed item and regroup only the
 * changed suffix. Transcript pages are bounded, so a prepend stays bounded too. */
export function incrementalHistoryGroups(
  items: Message[],
  currentTurn: string | undefined,
  prior: {
    items: Message[];
    currentTurn?: string;
    groups: HistoryGroup[];
  } | null,
): HistoryGroup[] {
  if (!prior || prior.currentTurn !== currentTurn)
    return historyGroups(items, currentTurn);
  let shared = 0;
  while (
    shared < items.length &&
    shared < prior.items.length &&
    items[shared] === prior.items[shared]
  )
    shared++;
  if (shared === items.length && shared === prior.items.length)
    return prior.groups;
  let sharedSuffix = 0;
  while (
    sharedSuffix < items.length - shared &&
    sharedSuffix < prior.items.length - shared &&
    items[items.length - 1 - sharedSuffix] ===
      prior.items[prior.items.length - 1 - sharedSuffix]
  )
    sharedSuffix++;
  let prefixGroups = 0;
  let prefixItems = 0;
  while (prefixGroups < prior.groups.length) {
    const length = prior.groups[prefixGroups].items.length;
    if (prefixItems + length > shared) break;
    prefixItems += length;
    prefixGroups++;
  }
  if (shared === prior.items.length && items.length > shared && prefixGroups)
    prefixGroups--;
  prefixItems = prior.groups
    .slice(0, prefixGroups)
    .reduce((count, group) => count + group.items.length, 0);
  let suffixGroups = prior.groups.length;
  let oldOffset = 0;
  const suffixStart = prior.items.length - sharedSuffix;
  for (let index = 0; index < prior.groups.length; index++) {
    oldOffset += prior.groups[index].items.length;
    if (oldOffset > suffixStart) {
      const boundary = prior.groups[index];
      const boundaryStart = oldOffset - boundary.items.length;
      // User rows always start a group and cannot join an adjacent group.
      suffixGroups =
        boundaryStart === suffixStart && boundary.items[0].role === "user"
          ? index
          : index + 1;
      break;
    }
  }
  suffixGroups = Math.max(prefixGroups, suffixGroups);
  const suffix = prior.groups.slice(suffixGroups);
  const suffixItems = suffix.reduce(
    (count, group) => count + group.items.length,
    0,
  );
  return [
    ...prior.groups.slice(0, prefixGroups),
    ...historyGroups(
      items.slice(prefixItems, items.length - suffixItems),
      currentTurn,
    ),
    ...suffix,
  ];
}
