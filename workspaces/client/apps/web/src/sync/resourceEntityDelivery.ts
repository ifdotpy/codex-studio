import type { ResourceChangeEvent } from "./resourceEvents";

const MAX_PENDING_FRAMES = 8;
const PERSIST_DEADLINE_MS = 1000;

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

  receive(event: ResourceChangeEvent) {
    const hasChanges = event.resourceVersions?.some(
      (entry) => entry.entityChanges,
    );
    const hasState = event.resources.some(
      (resource) => resource.kind === "state",
    );
    if (event.reason === "change" && !hasState && this.epoch === event.epoch) {
      this.dispatch(event);
      return;
    }
    if (
      event.reason !== "change" ||
      this.storageBlocked ||
      !hasChanges ||
      (this.epoch !== undefined && this.epoch !== event.epoch) ||
      this.pending.length >= MAX_PENDING_FRAMES
    ) {
      this.generation++;
      // Preserve all resource hints when data delivery falls back to pull.
      // A frame can also contain transcript, panel or queue invalidations.
      if (this.current) this.dispatch(this.current);
      for (const pending of this.pending) this.dispatch(pending);
      this.pending = [];
      this.epoch = event.epoch;
      this.dispatch(event);
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
          console.error("Stream entity persistence failed", error);
        } finally {
          clearTimeout(timer);
        }
        // A later baseline or overflow already requests the complete range.
        if (generation === this.generation) this.dispatch(event);
        this.current = undefined;
        if (expired) {
          for (const pending of this.pending) this.dispatch(pending);
          this.pending = [];
          break;
        }
      }
    } finally {
      this.running = false;
    }
  }
}
