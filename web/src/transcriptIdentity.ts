import type { Message } from "./types";

// Compare JSON values, including fields removed by the server. Deep or unusual
// values simply keep their new identity; this never discards incoming data.
function equalJson(a: unknown, b: unknown, depth = 0): boolean {
  if (Object.is(a, b)) return true;
  if (!a || !b || typeof a !== "object" || typeof b !== "object" || depth > 32)
    return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = Object.keys(left);
  return (
    keys.length === Object.keys(right).length &&
    keys.every(
      (key) =>
        Object.prototype.hasOwnProperty.call(right, key) &&
        equalJson(left[key], right[key], depth + 1),
    )
  );
}

// Full projections replace the wire payload. Unchanged rows in this same chat
// can retain their render caches without hiding updates, removals, or reordering.
export function retainTranscriptItems(
  previous: Message[],
  incoming: Message[],
): Message[] {
  const byId = new Map(previous.map((item) => [item.id, item]));
  const next = incoming.map((item) => {
    const prior = byId.get(item.id);
    return prior && equalJson(prior, item) ? prior : item;
  });
  return next.length === previous.length &&
    next.every((item, index) => item === previous[index])
    ? previous
    : next;
}
