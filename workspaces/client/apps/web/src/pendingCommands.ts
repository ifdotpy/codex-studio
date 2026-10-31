// The pending layer of the one command path (latency rule 1).
// Tracking: https://github.com/ifdotpy/layr-studio/issues/630
//
// The client makes the command id, so the UI shows the expected result at once (a queued
// message, a stop, an answer) as a pending entry keyed by that id. The entry is removed when
// the client holds a row whose `seq` is at or above the `seq` of the command response. The
// stream delivers rows in `seq` order, so the sequence checkpoint of the client is that
// proof. The response carries the changed rows, so the client does not wait for a frame.

export type PendingKind = "send" | "stop" | "withdraw" | "answer";

export type PendingEntry = {
  /** The command id. */
  readonly id: string;
  readonly kind: PendingKind;
  /** The chat that the command acts on. */
  readonly target: string;
  /** The message id of a withdraw. */
  readonly message?: string;
  /** Set when the response arrived: the entry waits for this stream sequence. */
  readonly responseSeq?: number;
};

/** An entry that the stream never confirms is removed after this time. Unit: milliseconds. */
export const PENDING_COMMAND_MAX_MS = 30_000;

type Timer = ReturnType<typeof setTimeout>;

export function createPendingCommands(
  options: {
    now?: () => number;
    setTimer?: (action: () => void, ms: number) => Timer;
    clearTimer?: (timer: Timer) => void;
  } = {},
) {
  const setTimer = options.setTimer ?? ((action, ms) => setTimeout(action, ms));
  const clearTimer = options.clearTimer ?? ((timer) => clearTimeout(timer));
  const entries = new Map<string, PendingEntry>();
  const timers = new Map<string, Timer>();
  const listeners = new Set<() => void>();
  let clientSeq = -1;
  let snapshot: readonly PendingEntry[] = [];

  function publish() {
    snapshot = [...entries.values()];
    for (const listener of listeners) listener();
  }

  function remove(id: string) {
    const timer = timers.get(id);
    if (timer !== undefined) clearTimer(timer);
    timers.delete(id);
    if (entries.delete(id)) publish();
  }

  return {
    /** Show the expected result before the request starts. */
    begin(entry: Omit<PendingEntry, "responseSeq">) {
      entries.set(entry.id, entry);
      timers.set(
        entry.id,
        setTimer(() => remove(entry.id), PENDING_COMMAND_MAX_MS),
      );
      publish();
    },
    /** The response arrived. Keep the entry until the client holds a row at `seq`. */
    settle(id: string, seq: number) {
      const entry = entries.get(id);
      if (!entry) return;
      if (seq <= clientSeq) return remove(id);
      entries.set(id, { ...entry, responseSeq: seq });
      publish();
    },
    /** The command failed. The real state shows through the stream. */
    fail(id: string) {
      remove(id);
    },
    /** The client now holds every row up to `seq`. */
    observeClientSeq(seq: number) {
      clientSeq = Math.max(clientSeq, seq);
      for (const entry of entries.values())
        if (entry.responseSeq !== undefined && entry.responseSeq <= clientSeq)
          remove(entry.id);
    },
    /** The sequence space changed (another workspace or epoch). */
    resetClientSeq() {
      clientSeq = -1;
    },
    list: () => snapshot,
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => void listeners.delete(listener);
    },
  };
}

export type PendingCommands = ReturnType<typeof createPendingCommands>;
