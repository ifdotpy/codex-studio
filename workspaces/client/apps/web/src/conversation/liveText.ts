import { useEffect, useState } from "react";
import { useServerResources } from "../servers/ServerServicesContext";

/** Text of one item that the server sent live, before the durable item holds it. `start` and
 * `length` count Unicode scalars, as the server does. */
export type LiveText = { start: number; length: number; text: string };

/** Items kept per chat; older ones are long durable. */
const MAX_LIVE_ITEMS = 8;

/** Add a delta to its item. A gap or a restart starts a new run at that offset. */
export function addLiveDelta(
  live: ReadonlyMap<string, LiveText>,
  delta: { itemId: string; offset: number; text: string },
): ReadonlyMap<string, LiveText> {
  const run = live.get(delta.itemId);
  const length = Array.from(delta.text).length;
  const next = new Map(live);
  next.delete(delta.itemId);
  next.set(
    delta.itemId,
    run && run.start + run.length === delta.offset
      ? { ...run, length: run.length + length, text: run.text + delta.text }
      : { start: delta.offset, length, text: delta.text },
  );
  for (const key of next.keys()) {
    if (next.size <= MAX_LIVE_ITEMS) break;
    next.delete(key);
  }
  return next;
}

/** The durable text with the live run on top of it. The durable text wins when it is at least as
 * long, or when it is too short to join the run. */
export function withLiveText(durable: string, live: LiveText | undefined) {
  if (!live) return durable;
  const scalars = Array.from(durable);
  if (scalars.length < live.start || scalars.length >= live.start + live.length)
    return durable;
  return scalars.slice(0, live.start).join("") + live.text;
}

/** Live text of the open chat, by item id. It is memory only and never written to a cache. */
export function useLiveText(agentId: string | undefined) {
  const { watchLiveDeltas } = useServerResources();
  const [live, setLive] = useState<ReadonlyMap<string, LiveText>>(new Map());
  useEffect(() => {
    setLive(new Map());
    if (!agentId) return;
    return watchLiveDeltas((event) => {
      if (event.agentId === agentId)
        setLive((current) => addLiveDelta(current, event));
    });
  }, [agentId, watchLiveDeltas]);
  return live;
}
