import type { ResourceChangeEvent } from "./subscriptions/events";
import { subscriptionKey } from "./subscriptions/contracts";

// Frames waiting for persistence. Accommodates four full 256-resource subscription
// bursts; the write deadline catches stuck storage before ordinary traffic fills it.
export const MAX_PENDING_RESOURCE_FRAMES = 1024;
const PERSIST_DEADLINE_MS = 1000;

type QueuedFrame = {
  event: ResourceChangeEvent;
  prerequisite?: Promise<boolean>;
};
type Application = QueuedFrame & { cancelled: boolean; release?: () => void };

/** One source's FIFO: persist each frame before publishing it. */
export class ResourceEntityDelivery {
  private pending: QueuedFrame[] = [];
  private current: Application | undefined;
  private running = false;
  private stopped = false;

  constructor(
    private readonly persist: (
      event: ResourceChangeEvent,
      isCurrent: () => boolean,
    ) => Promise<void>,
    private readonly dispatch: (event: ResourceChangeEvent) => void,
    private readonly failed?: (
      event: ResourceChangeEvent,
      error: unknown,
    ) => boolean | void,
  ) {}

  /** Source close silently cancels writes; the next source resumes durable cursors. */
  invalidate() {
    if (this.current) {
      this.current.cancelled = true;
      this.current.release?.();
    }
    this.current = undefined;
    this.pending = [];
    this.stopped = false;
  }

  /** Remove only the departed selectors; unrelated pending writes stay current. */
  remove(resources: ResourceChangeEvent["resources"]) {
    const removed = new Set(resources.map(subscriptionKey));
    const strip = (event: ResourceChangeEvent) => {
      const kept = event.resources.flatMap((resource, index) =>
        removed.has(subscriptionKey(resource)) ? [] : [index],
      );
      if (kept.length === event.resources.length) return false;
      event.resources = kept.map((index) => event.resources[index]!);
      event.resourceVersions = kept.map(
        (index) => event.resourceVersions[index]!,
      );
      return true;
    };
    for (const { event } of this.pending) strip(event);
    this.pending = this.pending.filter(({ event }) => event.resources.length);
    if (this.current && strip(this.current.event)) {
      const { event, prerequisite } = this.current;
      this.current.cancelled = true;
      this.current.release?.();
      this.current = undefined;
      // Retry retained entries after the cancelled operation settles. Persistence
      // recognizes already committed entries from their durable cursor/revision.
      if (event.resources.length) this.pending.unshift({ event, prerequisite });
    }
  }

  receive(event: ResourceChangeEvent, prerequisite?: Promise<unknown>) {
    if (this.stopped) return;
    this.pending.push({
      event,
      prerequisite: prerequisite?.then(
        () => true,
        () => false,
      ),
    });
    if (this.pending.length > MAX_PENDING_RESOURCE_FRAMES) {
      this.fail(
        new Error("Stream entity persistence queue exceeded its capacity"),
      );
      return;
    }
    void this.flush();
  }

  private fail(error: unknown) {
    const frames = [
      ...(this.current ? [this.current.event] : []),
      ...this.pending.map(({ event }) => event),
    ];
    const resources = [
      ...new Map(
        frames
          .flatMap((event) => event.resources)
          .map((resource) => [subscriptionKey(resource), resource]),
      ).values(),
    ];
    if (this.current) {
      this.current.cancelled = true;
      this.current.release?.();
    }
    this.current = undefined;
    this.pending = [];
    this.stopped = true;
    const handled = frames.length
      ? this.failed?.({ ...frames[0]!, resources }, error)
      : false;
    if (!this.failed || handled === false) this.stopped = false;
  }

  private async flush() {
    if (this.running) return;
    this.running = true;
    try {
      while (!this.stopped && this.pending.length) {
        const queued = this.pending.shift()!;
        const { event, prerequisite } = queued;
        const application: Application = { ...queued, cancelled: false };
        this.current = application;
        let timer: ReturnType<typeof setTimeout> | undefined;
        const isCurrent = () => !application.cancelled;
        if (prerequisite) {
          const ready = await Promise.race([
            prerequisite,
            new Promise<false>((resolve) => {
              application.release = () => resolve(false);
            }),
          ]);
          if (!isCurrent()) continue;
          application.release = undefined;
          // Transfer errors already have a recovery owner; never fail twice.
          if (!ready) {
            this.current = undefined;
            continue;
          }
        }
        try {
          await Promise.race([
            this.persist(event, isCurrent),
            new Promise<never>((_, reject) => {
              timer = setTimeout(
                () => reject(new Error("Stream entity persistence timed out")),
                PERSIST_DEADLINE_MS,
              );
            }),
          ]);
        } catch (error) {
          if (isCurrent()) this.fail(error);
          continue;
        } finally {
          clearTimeout(timer);
        }
        if (isCurrent()) {
          this.current = undefined;
          this.dispatch(event);
        }
      }
    } finally {
      this.running = false;
    }
  }
}
