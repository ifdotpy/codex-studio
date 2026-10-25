import type { ResourceChangeEvent } from "./subscriptions/events";

const MAX_PENDING_FRAMES = 8;
const PERSIST_DEADLINE_MS = 1000;

function revokeCursorProof(event: ResourceChangeEvent) {
  for (const entry of event.resourceVersions ?? []) {
    if (entry.replyKind === "delta" && entry.viewSnapshot?.terminalOutput) {
      // Without a durable base this is an interval, not a complete view.
      entry.viewSnapshot = { ...entry.viewSnapshot };
      delete entry.viewSnapshot.terminalOutput;
    }
    delete entry.cursor;
    delete entry.replyKind;
  }
  return event;
}

/** Bound durable stream writes and retain invalidations when writes cannot keep up. */
export class ResourceEntityDelivery {
  private pending: ResourceChangeEvent[] = [];
  private generation = 0;
  private epoch: string | undefined;
  private running = false;
  private current: ResourceChangeEvent | undefined;
  private storageBlocked = false;

  constructor(
    private readonly persist: (
      event: ResourceChangeEvent,
      isCurrent: () => boolean,
    ) => Promise<void>,
    private readonly dispatch: (event: ResourceChangeEvent) => void,
  ) {}

  /** Cancel reads from an earlier subscription set and retain active pull hints. */
  invalidate() {
    this.generation++;
    if (this.current) this.dispatch(revokeCursorProof(this.current));
    for (const pending of this.pending)
      this.dispatch(revokeCursorProof(pending));
    this.pending = [];
  }

  receive(event: ResourceChangeEvent) {
    const hasInitialSnapshot =
      (event.reason === "initial" || event.reason === "reconnect") &&
      event.resourceVersions?.some(
        (entry) =>
          entry.transcriptSnapshot || (entry.cursor && entry.replyKind),
      );
    const hasChanges = event.resourceVersions?.some(
      (entry) =>
        entry.entityChanges ||
        entry.transcriptChanges ||
        entry.transcriptSnapshot ||
        (entry.cursor && entry.replyKind),
    );
    const hasState = event.resources.some(
      (resource) => resource.kind === "state",
    );
    if (
      event.reason === "change" &&
      !hasState &&
      !hasChanges &&
      this.epoch === event.epoch
    ) {
      this.dispatch(event);
      return;
    }
    if (
      (event.reason !== "change" && !hasInitialSnapshot) ||
      this.storageBlocked ||
      !hasChanges ||
      (this.epoch !== undefined &&
        this.epoch !== event.epoch &&
        !hasInitialSnapshot) ||
      this.pending.length >= MAX_PENDING_FRAMES
    ) {
      this.generation++;
      // Preserve all resource hints when data delivery falls back to pull.
      // A frame can also contain transcript, panel or queue invalidations.
      if (this.current) this.dispatch(revokeCursorProof(this.current));
      for (const pending of this.pending)
        this.dispatch(revokeCursorProof(pending));
      this.pending = [];
      this.epoch = event.epoch;
      this.dispatch(revokeCursorProof(event));
      return;
    }
    this.epoch = event.epoch;
    this.pending.push(event);
    void this.flush();
  }

  private async flush() {
    if (this.running) return;
    this.running = true;
    try {
      for (;;) {
        const event = this.pending.shift();
        if (!event) break;
        this.current = event;
        const generation = this.generation;
        let expired = false;
        let timer: ReturnType<typeof setTimeout> | undefined;
        const isCurrent = () => !expired && generation === this.generation;
        try {
          const persistence = this.persist(event, isCurrent);
          void persistence.then(
            () => {
              if (expired) this.storageBlocked = false;
            },
            () => {
              if (expired) this.storageBlocked = false;
            },
          );
          await Promise.race([
            persistence,
            new Promise<never>((_, reject) => {
              timer = setTimeout(() => {
                expired = true;
                this.storageBlocked = true;
                reject(new Error("Stream entity persistence timed out"));
              }, PERSIST_DEADLINE_MS);
            }),
          ]);
        } catch (error) {
          revokeCursorProof(event);
          console.error("Stream entity persistence failed", error);
        } finally {
          clearTimeout(timer);
        }
        // A later baseline or overflow already requests the complete range.
        if (generation === this.generation) this.dispatch(event);
        this.current = undefined;
        if (expired) {
          for (const pending of this.pending)
            this.dispatch(revokeCursorProof(pending));
          this.pending = [];
          break;
        }
      }
    } finally {
      this.running = false;
    }
  }
}
