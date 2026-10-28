import { pageCore } from "../native/client";
import { NativeEventSource } from "../native/stream";

/** A lost native connection; the core reconnects at `retryAt` (epoch ms). */
export type NativeStreamRetry = { opened: boolean; retryAt: number };
type EventSourceHandlers = {
  [K in "onopen" | "onerror"]: OmitThisParameter<
    NonNullable<EventSource[K]>
  > | null;
};
export type ServerEventSource = Pick<EventSource, "close"> &
  EventSourceHandlers &
  Pick<EventTarget, "addEventListener"> & {
    holdUntil?(ready: Promise<unknown>): void;
    /**
     * Only a native source has these. The native core reconnects by itself
     * (see native/stream.ts); the page reports, answers and never retries.
     */
    onretry?: ((detail: NativeStreamRetry) => void) | null;
    onresume?: (() => void) | null;
    resume?(path: string): void;
    restart?(reason: string): void;
  };
export function openServerEvents(url: string): ServerEventSource {
  // The page's own client core opens and resumes the stream.
  return new NativeEventSource(url, pageCore());
}
