import type { components } from "../../generated/api";
import { SYNC_TERMINAL_STREAM_MAX_CHARS } from "../../generated/sharedValues";
type Output = components["schemas"]["TerminalOutput"];

/** Offsets count Unicode scalars, matching the persisted Rust terminal buffer. */
export function mergeTerminalOutput(
  previous: Output | undefined,
  incoming: Output,
  delta: boolean,
): Output {
  const next = Array.from(incoming.text);
  const start = incoming.offset - next.length;
  if (!Number.isSafeInteger(incoming.offset) || start < 0)
    throw new Error("Invalid terminal output interval");
  if (delta && (!previous || previous.offset < start))
    throw new Error("Terminal output cursor has no local base");
  if (!delta)
    return {
      ...incoming,
      text: next.slice(-SYNC_TERMINAL_STREAM_MAX_CHARS).join(""),
      truncated:
        incoming.truncated || next.length > SYNC_TERMINAL_STREAM_MAX_CHARS,
    };
  if (previous && incoming.offset < previous.offset) return previous;
  const old = previous ? Array.from(previous.text) : [];
  const previousStart = previous ? previous.offset - old.length : start;
  const joined =
    previous && previous.offset >= start && previousStart <= start
      ? [...old.slice(0, start - previousStart), ...next]
      : next;
  const text = joined.slice(-SYNC_TERMINAL_STREAM_MAX_CHARS).join("");
  return {
    ...incoming,
    text,
    truncated:
      incoming.truncated ||
      previous?.truncated === true ||
      joined.length > SYNC_TERMINAL_STREAM_MAX_CHARS,
  };
}
