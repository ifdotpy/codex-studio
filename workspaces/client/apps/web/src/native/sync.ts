import type { NativeServerContext } from "./client";
import type { createNativeSubscriptions } from "./subscriptions";
import type { ResourceRef } from "../sync/subscriptions/contracts";
import type { Snapshot, TranscriptPageData } from "../types";
import type { UiPreferenceProjection } from "../sync/entityProjection";
import type { components } from "../generated/api";

export type NativeSnapshot = Omit<Snapshot, "token">;
export type NativeSyncContext = Pick<
  NativeServerContext,
  "workspaceId" | "request" | "subscribe"
>;
export type NativeSyncResources = Pick<
  ReturnType<typeof createNativeSubscriptions>,
  "subscribe" | "observe" | "getSnapshot"
>;

type CommittedSubscription = {
  data?: unknown;
  status: "ready" | "recovery_required";
  cursor?: components["schemas"]["SubscriptionCursor"];
};

/** Native code owns durable data, stream recovery and network deadlines. */
export function createNativeSync(
  context: Promise<NativeSyncContext>,
  resources: NativeSyncResources,
) {
  let workspace: Promise<string> | undefined;
  let disposed = false;
  const stops = new Set<() => void>();
  const pending = new Set<Promise<void>>();
  const operationErrors: unknown[] = [];
  const getWorkspaceId = () =>
    (workspace ??= context.then((owner) => owner.workspaceId));
  function ensureOpen() {
    if (disposed) throw new Error("The native sync adapter is closed.");
  }
  function track(operation: Promise<void>, report: (error: unknown) => void) {
    pending.add(operation);
    void operation.then(
      () => pending.delete(operation),
      (error) => {
        pending.delete(operation);
        operationErrors.push(error);
        report(error);
      },
    );
  }
  return {
    get workspaceId() {
      return getWorkspaceId();
    },
    getWorkspaceId,
    subscribeStateProjection(
      accept: (snapshot: NativeSnapshot | null, workspaceId: string) => void,
      report: (error: unknown | null) => void,
      _preferences?: (preferences: UiPreferenceProjection) => void,
    ) {
      ensureOpen();
      let stopped = false;
      let close: (() => Promise<void>) | undefined;
      const stop = () => {
        if (stopped) return;
        stopped = true;
        stops.delete(stop);
        if (close) track(close(), report);
      };
      stops.add(stop);
      track(
        context.then(async (owner) => {
          if (stopped) return;
          const opened = await owner.subscribe<NativeSnapshot | null>(
            "state",
            {},
            (event) => {
              if (stopped) return;
              if (event.error) report(new Error(event.error.message));
              else if (Object.hasOwn(event, "value")) {
                accept(event.value ?? null, owner.workspaceId);
                report(null);
              }
            },
          );
          if (stopped) await opened();
          else close = opened;
        }),
        (error) => {
          if (!stopped) report(error);
        },
      );
      return stop;
    },
    subscribeTranscriptProjection(
      scope: `transcript:${string}`,
      accept: (data: TranscriptPageData | null) => void,
      report: (error: unknown | null) => void,
      localOnly = false,
      resource?: Extract<ResourceRef, { kind: "transcript" }>,
    ) {
      ensureOpen();
      const agentId = scope.slice("transcript:".length);
      if (
        !scope.startsWith("transcript:") ||
        !agentId ||
        (resource && resource.agentId !== agentId)
      )
        throw new Error(
          "The transcript resource identity does not match its scope.",
        );
      const ref = resource ?? { kind: "transcript", agentId };
      let stopped = false;
      let observed = false;
      const receive = () => {
        if (stopped) return;
        const value = resources.getSnapshot(ref);
        if (value.error) {
          report(value.error);
          return;
        }
        if (value.data !== undefined) {
          observed = true;
          accept(value.data as TranscriptPageData | null);
          report(null);
        }
      };
      const release =
        localOnly && !resource
          ? resources.observe(ref, receive)
          : resources.subscribe(ref, receive);
      const stop = () => {
        if (stopped) return;
        stopped = true;
        stops.delete(stop);
        release();
      };
      stops.add(stop);
      receive();
      track(
        context.then(async (owner) => {
          if (stopped) return;
          const saved = await owner.request<CommittedSubscription>(
            "projection.readSubscription",
            { workspace: owner.workspaceId, resource: ref },
          );
          if (!stopped && !observed && saved.data !== undefined) {
            accept(saved.data as TranscriptPageData | null);
            report(null);
          }
        }),
        (error) => {
          if (!stopped) report(error);
        },
      );
      return stop;
    },
    async refreshProjection(
      options: {
        afterCurrentPull?: boolean;
        onWorkspaceId?: (workspace: string) => void;
      } = {},
    ): Promise<NativeSnapshot> {
      ensureOpen();
      const owner = await context;
      const result = await owner.request<CommittedSubscription>(
        "subscriptions.snapshot",
        { resource: { kind: "state" } },
      );
      if (result.status !== "ready")
        throw new Error(
          "The native state snapshot needs recovery before it is ready.",
        );
      const value = await owner.request<NativeSnapshot | null>(
        "projection.stateSnapshot",
        { workspace: owner.workspaceId },
      );
      if (value === null)
        throw new Error("The native state snapshot is not ready.");
      ensureOpen();
      options.onWorkspaceId?.(owner.workspaceId);
      return value;
    },
    async prefetchTranscript(
      expectedWorkspace: string,
      id: string,
      signal?: AbortSignal,
    ): Promise<boolean> {
      ensureOpen();
      if (signal?.aborted || !id) return false;
      const owner = await context;
      if (signal?.aborted || owner.workspaceId !== expectedWorkspace)
        return false;
      const result = await owner.request<CommittedSubscription>(
        "subscriptions.snapshot",
        { resource: { kind: "transcript", agentId: id } },
      );
      return !signal?.aborted && !disposed && result.status === "ready";
    },
    async readSubscriptionCursor(
      resource: ResourceRef,
    ): Promise<components["schemas"]["SubscriptionCursor"] | undefined> {
      ensureOpen();
      const owner = await context;
      const result = await owner.request<CommittedSubscription>(
        "projection.readSubscription",
        { workspace: owner.workspaceId, resource },
      );
      return result.cursor?.workspaceId === owner.workspaceId
        ? result.cursor
        : undefined;
    },
    async dispose() {
      if (disposed) return;
      disposed = true;
      for (const stop of stops) stop();
      while (pending.size) await Promise.allSettled(pending);
      if (operationErrors.length)
        throw new AggregateError(
          operationErrors,
          "A native sync operation failed.",
        );
    },
  };
}
