import type { ResourceChangeEvent } from "./subscriptions/events";
import { subscriptionKey } from "./subscriptions/contracts";

<<<<<<< head
// Frames waiting for persistence. Accommodates four full 256-resource subscription
// bursts; the write deadline catches stuck storage before ordinary traffic fills it.
export const MAX_PENDING_RESOURCE_FRAMES = 1024;
// Time one frame may take to reach storage, in milliseconds. WebKit storage
// took over a second for startup snapshots in the desktop app (2026-10-26).
||||||| base
const MAX_PENDING_FRAMES = 8;
const PERSIST_DEADLINE_MS = 1000;
=======
/** Frames that may wait while one frame is written, in frames. A startup
 * burst sends one frame per subscribed resource; each inline frame is at most
 * one SSE event (SYNC_STREAM_EVENT_MAX_BYTES), so the queue stays bounded. */
export const MAX_PENDING_FRAMES = 128;
/** Time one frame may take to reach storage, in milliseconds. WebKit storage
 * can take seconds for a startup snapshot; a short limit blocked storage and
 * discarded every later frame. */
>>>>>>> 686377a
export const PERSIST_DEADLINE_MS = 10_000;

type QueuedFrame = {
  event: ResourceChangeEvent;
  prerequisite?: Promise<boolean>;
  operation?: {
    persist(isCurrent: () => boolean): Promise<void>;
    dispose(): void;
  };
};
type Application = QueuedFrame & { cancelled: boolean; release?: () => void };

/** One source's FIFO: persist each frame before publishing it. */
export class ResourceEntityDelivery {
  private pending: QueuedFrame[] = [];
  private current: Application | undefined;
  private running = false;
<<<<<<< head
  private stopped = false;
||||||| base
  private current: ResourceChangeEvent | undefined;
  private storageBlocked = false;
  // Saved output alone cannot prove a baseline under the current stream owner.
  private terminalBaselines = new Set<string>();
  // A skipped snapshot also fences intervals already beyond the old offset.
  private terminalSnapshotsRequired = new Map<
    string,
    { epoch: string; revision: number }
  >();

  requiresSnapshot(resource: ResourceChangeEvent["resources"][number]) {
    const key = subscriptionKey(resource);
    return (
      resource.kind === "terminal-output" &&
      (!this.terminalBaselines.has(key) ||
        this.terminalSnapshotsRequired.has(key))
    );
  }

  /** A new stream owner cannot inherit a follower's unproven terminal base. */
  beginOwnership() {
    this.invalidate();
    this.terminalBaselines.clear();
  }

  private revoke(event: ResourceChangeEvent) {
    for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
      const resource = entry.resource ?? event.resources[index];
      if (
        resource?.kind !== "terminal-output" ||
        entry.replyKind !== "snapshot" ||
        !entry.viewSnapshot?.terminalOutput
      )
        continue;
      const key = subscriptionKey(resource);
      const required = this.terminalSnapshotsRequired.get(key);
      if (
        !required ||
        required.epoch !== event.epoch ||
        required.revision < entry.revision
      )
        this.terminalSnapshotsRequired.set(key, {
          epoch: event.epoch,
          revision: entry.revision,
        });
    }
    return revokeCursorProof(event);
  }

  private restoreTerminalBases(event: ResourceChangeEvent) {
    for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
      const resource = entry.resource ?? event.resources[index];
      if (
        resource?.kind !== "terminal-output" ||
        entry.replyKind !== "snapshot" ||
        !entry.cursor
      )
        continue;
      const key = subscriptionKey(resource);
      const required = this.terminalSnapshotsRequired.get(key);
      if (
        !required ||
        required.epoch !== event.epoch ||
        entry.revision >= required.revision
      ) {
        this.terminalSnapshotsRequired.delete(key);
        this.terminalBaselines.add(key);
      }
    }
  }
=======
  private current: ResourceChangeEvent | undefined;
  private storageBlocked = false;
  // Saved output alone cannot prove a baseline under the current stream owner.
  private terminalBaselines = new Set<string>();
  // A skipped snapshot also fences intervals already beyond the old offset.
  private terminalSnapshotsRequired = new Map<
    string,
    { epoch: string; revision: number }
  >();

  // Transcripts whose delta failed for lack of a local base. Their next
  // subscription asks for a snapshot; resuming from the cursor repeats the gap.
  private transcriptSnapshotsRequired = new Set<string>();

  requiresSnapshot(resource: ResourceChangeEvent["resources"][number]) {
    const key = subscriptionKey(resource);
    if (resource.kind === "transcript")
      return this.transcriptSnapshotsRequired.has(key);
    return (
      resource.kind === "terminal-output" &&
      (!this.terminalBaselines.has(key) ||
        this.terminalSnapshotsRequired.has(key))
    );
  }

  private trackTranscriptBases(event: ResourceChangeEvent, failed: boolean) {
    for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
      const resource = entry.resource ?? event.resources[index];
      if (resource?.kind !== "transcript") continue;
      const key = subscriptionKey(resource);
      if (failed) this.transcriptSnapshotsRequired.add(key);
      else if (entry.transcriptSnapshot)
        this.transcriptSnapshotsRequired.delete(key);
    }
  }

  /** A new stream owner cannot inherit a follower's unproven terminal base. */
  beginOwnership() {
    this.invalidate();
    this.terminalBaselines.clear();
  }

  private revoke(event: ResourceChangeEvent) {
    for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
      const resource = entry.resource ?? event.resources[index];
      if (
        resource?.kind !== "terminal-output" ||
        entry.replyKind !== "snapshot" ||
        !entry.viewSnapshot?.terminalOutput
      )
        continue;
      const key = subscriptionKey(resource);
      const required = this.terminalSnapshotsRequired.get(key);
      if (
        !required ||
        required.epoch !== event.epoch ||
        required.revision < entry.revision
      )
        this.terminalSnapshotsRequired.set(key, {
          epoch: event.epoch,
          revision: entry.revision,
        });
    }
    return revokeCursorProof(event);
  }

  private restoreTerminalBases(event: ResourceChangeEvent) {
    for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
      const resource = entry.resource ?? event.resources[index];
      if (
        resource?.kind !== "terminal-output" ||
        entry.replyKind !== "snapshot" ||
        !entry.cursor
      )
        continue;
      const key = subscriptionKey(resource);
      const required = this.terminalSnapshotsRequired.get(key);
      if (
        !required ||
        required.epoch !== event.epoch ||
        entry.revision >= required.revision
      ) {
        this.terminalSnapshotsRequired.delete(key);
        this.terminalBaselines.add(key);
      }
    }
  }
>>>>>>> 686377a

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
    for (const queued of this.pending) queued.operation?.dispose();
    this.pending = [];
<<<<<<< head
    this.stopped = false;
||||||| base
    if (recover && this.recoverDiscarded(discarded)) return;
    for (const event of discarded) this.dispatch(this.revoke(event));
  }

  private recoverDiscarded(frames: ResourceChangeEvent[]): boolean {
    const typed = frames.filter((frame) =>
      frame.resourceVersions?.some(
        (entry) =>
          entry.cursor ||
          entry.entityChanges ||
          entry.transcriptChanges ||
          entry.transcriptSnapshot,
      ),
    );
    if (!this.failed || !typed.length) return false;
    const resources = [
      ...new Map(
        typed
          .flatMap((frame) => frame.resources)
          .map((ref) => [subscriptionKey(ref), ref]),
      ).values(),
    ];
    for (const frame of typed) this.revoke(frame);
    return (
      this.failed(
        { ...typed[0], resources },
        new Error(
          "Subscribed entity interval was discarded before application",
        ),
      ) !== false
    );
  }

  private hasPendingTerminalSnapshot(
    resource: ResourceChangeEvent["resources"][number],
    event: ResourceChangeEvent,
  ) {
    const key = subscriptionKey(resource);
    return [this.current, ...this.pending].some(
      (frame) =>
        frame?.epoch === event.epoch &&
        frame.workspaceId === event.workspaceId &&
        frame.resourceVersions?.some((entry, index) => {
          const reference = entry.resource ?? frame.resources[index];
          return (
            reference &&
            subscriptionKey(reference) === key &&
            entry.replyKind === "snapshot" &&
            entry.cursor &&
            entry.viewSnapshot?.terminalOutput
          );
        }),
    );
=======
    if (
      recover &&
      this.recoverDiscarded(discarded, "a subscription was removed")
    )
      return;
    for (const event of discarded) this.dispatch(this.revoke(event));
  }

  private recoverDiscarded(
    frames: ResourceChangeEvent[],
    cause: string,
  ): boolean {
    const typed = frames.filter((frame) =>
      frame.resourceVersions?.some(
        (entry) =>
          entry.cursor ||
          entry.entityChanges ||
          entry.transcriptChanges ||
          entry.transcriptSnapshot,
      ),
    );
    if (!this.failed || !typed.length) return false;
    const resources = [
      ...new Map(
        typed
          .flatMap((frame) => frame.resources)
          .map((ref) => [subscriptionKey(ref), ref]),
      ).values(),
    ];
    for (const frame of typed) this.revoke(frame);
    return (
      this.failed(
        { ...typed[0], resources },
        new Error(
          `Subscribed entity interval was discarded before application: ${cause}`,
        ),
      ) !== false
    );
  }

  private hasPendingTerminalSnapshot(
    resource: ResourceChangeEvent["resources"][number],
    event: ResourceChangeEvent,
  ) {
    const key = subscriptionKey(resource);
    return [this.current, ...this.pending].some(
      (frame) =>
        frame?.epoch === event.epoch &&
        frame.workspaceId === event.workspaceId &&
        frame.resourceVersions?.some((entry, index) => {
          const reference = entry.resource ?? frame.resources[index];
          return (
            reference &&
            subscriptionKey(reference) === key &&
            entry.replyKind === "snapshot" &&
            entry.cursor &&
            entry.viewSnapshot?.terminalOutput
          );
        }),
    );
>>>>>>> 686377a
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
    this.pending = this.pending.filter((queued) => {
      if (queued.event.resources.length) return true;
      queued.operation?.dispose();
      return false;
    });
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

  receive(
    event: ResourceChangeEvent,
    prerequisite?: Promise<unknown>,
    operation?: QueuedFrame["operation"],
  ) {
    if (this.stopped) {
      operation?.dispose();
      return;
    }
<<<<<<< head
    this.pending.push({
      event,
      operation,
      prerequisite: prerequisite?.then(
        () => true,
        () => false,
      ),
    });
    if (this.pending.length > MAX_PENDING_RESOURCE_FRAMES) {
      this.fail(
        new Error("Stream entity persistence queue exceeded its capacity"),
      );
||||||| base
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
      const discarded = [
        ...(this.current ? [this.current] : []),
        ...this.pending,
        event,
      ];
      this.current = undefined;
      this.pending = [];
      this.epoch = event.epoch;
      if (!this.recoverDiscarded(discarded))
        for (const frame of discarded) this.dispatch(this.revoke(frame));
=======
    if (
      !hasChanges &&
      !this.storageBlocked &&
      (this.current || this.pending.length) &&
      this.pending.length < MAX_PENDING_FRAMES &&
      (this.epoch === undefined || this.epoch === event.epoch)
    ) {
      // A frame without data cannot replace data that waits to be written,
      // for example the state frame whose data comes as a staged transfer.
      this.pending.push(event);
      void this.flush();
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
      const cause = this.storageBlocked
        ? "storage is blocked"
        : this.pending.length >= MAX_PENDING_FRAMES
          ? "too many frames wait"
          : !hasChanges
            ? "the frame has no changes"
            : "the frame has no initial snapshot";
      // Preserve all resource hints when data delivery falls back to pull.
      // A frame can also contain transcript, panel or queue invalidations.
      const discarded = [
        ...(this.current ? [this.current] : []),
        ...this.pending,
        event,
      ];
      this.current = undefined;
      this.pending = [];
      this.epoch = event.epoch;
      if (!this.recoverDiscarded(discarded, cause))
        for (const frame of discarded) this.dispatch(this.revoke(frame));
>>>>>>> 686377a
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
    for (const queued of this.pending) queued.operation?.dispose();
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
        try {
<<<<<<< head
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
              queued.operation
                ? queued.operation.persist(isCurrent)
                : this.persist(event, isCurrent),
              new Promise<never>((_, reject) => {
                timer = setTimeout(
                  () =>
                    reject(new Error("Stream entity persistence timed out")),
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
||||||| base
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
          persistenceFailed = true;
          persistenceError = error;
          this.revoke(event);
          console.error("Stream entity persistence failed", error);
=======
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
          persistenceFailed = true;
          persistenceError = error;
          this.trackTranscriptBases(event, true);
          this.revoke(event);
          console.error("Stream entity persistence failed", error);
>>>>>>> 686377a
        } finally {
<<<<<<< head
          queued.operation?.dispose();
||||||| base
          clearTimeout(timer);
        }
        // A later baseline or overflow already requests the complete range.
        if (isCurrent()) this.restoreTerminalBases(event);
        this.current = undefined;
        if (generation === this.generation) {
          if (persistenceFailed && this.failed) {
            // A fresh subscription must recover the whole missed interval.
            // Queued deltas have no applied base and cannot prove liveness.
            for (const pending of this.pending) this.revoke(pending);
            this.pending = [];
            if (this.failed(event, persistenceError) === false)
              this.dispatch(event);
          } else this.dispatch(event);
        }
        if (expired) {
          const discarded = this.pending;
          this.pending = [];
          if (!this.recoverDiscarded(discarded))
            for (const pending of discarded)
              this.dispatch(this.revoke(pending));
          break;
=======
          clearTimeout(timer);
        }
        // A later baseline or overflow already requests the complete range.
        if (isCurrent()) this.restoreTerminalBases(event);
        if (!persistenceFailed) this.trackTranscriptBases(event, false);
        this.current = undefined;
        if (generation === this.generation) {
          if (persistenceFailed && this.failed) {
            // A fresh subscription must recover the whole missed interval.
            // Queued deltas have no applied base and cannot prove liveness.
            for (const pending of this.pending) this.revoke(pending);
            this.pending = [];
            if (this.failed(event, persistenceError) === false)
              this.dispatch(event);
          } else this.dispatch(event);
        }
        if (expired) {
          const discarded = this.pending;
          this.pending = [];
          if (!this.recoverDiscarded(discarded, "a write passed its deadline"))
            for (const pending of discarded)
              this.dispatch(this.revoke(pending));
          break;
>>>>>>> 686377a
        }
      }
    } finally {
      this.running = false;
    }
  }
}
