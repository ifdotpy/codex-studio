import type { ResourceChangeEvent } from "./subscriptions/events";
import { subscriptionKey } from "./subscriptions/contracts";

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

  /** Cancel reads from an earlier subscription set and retain active pull hints. */
  invalidate(recover = false) {
    this.generation++;
    const discarded = [
      ...(this.current ? [this.current] : []),
      ...this.pending,
    ];
    this.current = undefined;
    this.pending = [];
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
  }

  private withholdUnsafeTerminalIntervals(
    event: ResourceChangeEvent,
    waitForPendingSnapshot = false,
  ) {
    for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
      const resource = entry.resource ?? event.resources[index];
      if (resource?.kind !== "terminal-output") continue;
      const required = this.terminalSnapshotsRequired.get(
        subscriptionKey(resource),
      );
      if (
        (entry.replyKind === "delta" &&
          this.requiresSnapshot(resource) &&
          !(
            waitForPendingSnapshot &&
            this.hasPendingTerminalSnapshot(resource, event)
          )) ||
        (required &&
          entry.replyKind === "snapshot" &&
          required.epoch === event.epoch &&
          entry.revision < required.revision)
      ) {
        // Keep only a scoped recovery hint until the skipped snapshot's base
        // commits; an old snapshot replay cannot satisfy that requirement.
        if (entry.viewSnapshot?.terminalOutput) {
          entry.viewSnapshot = { ...entry.viewSnapshot };
          delete entry.viewSnapshot.terminalOutput;
        }
        revokeCursorProof({ ...event, resourceVersions: [entry] });
      }
    }
  }

  receive(event: ResourceChangeEvent) {
    this.withholdUnsafeTerminalIntervals(event, true);
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
    if (this.storageBlocked && hasChanges && this.failed) {
      // A timed-out write may still own storage. Keep the stream degraded and
      // its reconnect bounded until that write settles; never confirm a hint.
      this.failed(
        this.revoke(event),
        new Error("Stream entity storage is still unavailable"),
      );
      return;
    }
    if (
      (event.reason === "change" || event.reason === "initial") &&
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
        // A reset may have failed while this interval was already queued.
        this.withholdUnsafeTerminalIntervals(event);
        this.current = event;
        const generation = this.generation;
        let expired = false;
        let persistenceFailed = false;
        let persistenceError: unknown;
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
          persistenceFailed = true;
          persistenceError = error;
          this.revoke(event);
          console.error("Stream entity persistence failed", error);
        } finally {
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
        }
      }
    } finally {
      this.running = false;
    }
  }
}
