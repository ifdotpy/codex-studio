import type { NativeServerContext, NativeSubscriptionEvent } from "./client";
import type { ResourceConnectionState, ResourceStreamState, ResourceVersion, ResourceTokenRatesEvent } from "../sync/resourceEvents";
import { subscriptionKey, type ResourceRef, type ResourceData, type SubscriptionState } from "../sync/subscriptions/contracts";

type NativePort = Pick<NativeServerContext, "subscribe">;
type Snapshot = { resource: ResourceRef; data?: unknown; status: "ready" | "recovery_required"; version?: ResourceVersion | null };
type ViewMessage =
  | { type: "resource"; snapshot: Snapshot }
  | { type: "connection"; status: ResourceConnectionState; stream: ResourceStreamState; message?: string }
  | { type: "token-rates"; value: ResourceTokenRatesEvent };
type Listener = (version?: ResourceVersion) => void;
type Entry = {
  ref: ResourceRef;
  state: SubscriptionState<unknown>;
  version?: ResourceVersion;
  changes: Set<Listener>;
  observers: Set<() => void>;
  close?: () => Promise<void>;
  opening: boolean;
  generation: number;
};

/** React retains presentation values. Rust owns transport, persistence and readiness. */
export function createNativeSubscriptions(context: Promise<NativePort>) {
  const entries = new Map<string, Entry>();
  const connectionListeners = new Set<(status: ResourceConnectionState, stream: ResourceStreamState) => void>();
  const tokenListeners = new Set<(value: ResourceTokenRatesEvent) => void>();
  const pending = new Set<Promise<unknown>>();
  const closeErrors: unknown[] = [];
  let connection: ResourceConnectionState = "connecting";
  let stream: ResourceStreamState = "stopped";
  let disposed = false;
  let tokenGeneration = 0;
  let closeTokens: (() => Promise<void>) | undefined;
  let lastTokens: ResourceTokenRatesEvent | undefined;

  function track<T>(operation: Promise<T>) {
    pending.add(operation);
    void operation.then(() => pending.delete(operation), () => pending.delete(operation));
    return operation;
  }
  function closeView(close: () => Promise<void>, entry?: Entry) {
    const operation = track(close());
    void operation.catch((error) => {
      closeErrors.push(error);
      if (!disposed && entry) report(entry, error);
    });
    return operation;
  }

  function entryFor(ref: ResourceRef) {
    const key = subscriptionKey(ref);
    let entry = entries.get(key);
    if (!entry) {
      entry = { ref, state: { data: undefined, status: "loading", error: null }, changes: new Set(), observers: new Set(), opening: false, generation: 0 };
      entries.set(key, entry);
    }
    return entry;
  }
  function notify(entry: Entry) {
    for (const listener of entry.observers) listener();
    for (const listener of entry.changes) listener(entry.version);
  }
  function report(entry: Entry, error: unknown) {
    entry.state = { ...entry.state, status: "error", error: error instanceof Error ? error : new Error(String(error)) };
    notify(entry);
  }
  function updateConnection(status: ResourceConnectionState, next: ResourceStreamState) {
    connection = status;
    stream = next;
    for (const listener of connectionListeners) listener(status, next);
  }
  function receive(entry: Entry, event: NativeSubscriptionEvent<ViewMessage>) {
    if (event.error) { report(entry, new Error(event.error.message)); return; }
    const value = event.value;
    if (!value) return;
    if (value.type === "connection") {
      updateConnection(value.status, value.stream);
      if (value.status !== "live" && entry.state.data !== undefined) {
        entry.state = { ...entry.state, status: "stale" };
        notify(entry);
      }
    } else if (value.type === "resource") {
      if (subscriptionKey(value.snapshot.resource) !== subscriptionKey(entry.ref)) {
        report(entry, new Error("The native view returned another resource."));
        return;
      }
      entry.version = value.snapshot.version ?? undefined;
      entry.state = {
        data: value.snapshot.data,
        status: value.snapshot.status === "ready" ? "live" : value.snapshot.data === undefined ? "loading" : "stale",
        error: null,
      };
      notify(entry);
    }
  }
  function start(entry: Entry) {
    if (entry.opening || entry.close || disposed) return;
    entry.opening = true;
    const generation = ++entry.generation;
    void track(context.then((native) => native.subscribe<ViewMessage>("subscriptions", { resources: [entry.ref], tokenRates: false }, (event) => {
      if (!disposed && entry.generation === generation) receive(entry, event);
    })).then((close) => {
      if (disposed || entry.generation !== generation) return closeView(close, entry);
      else { entry.opening = false; entry.close = close; }
    }).catch((error) => {
      if (!disposed && entry.generation === generation) { entry.opening = false; report(entry, error); }
    }));
  }
  function release(entry: Entry) {
    entry.generation++;
    entry.opening = false;
    const close = entry.close;
    entry.close = undefined;
    if (close) void closeView(close, entry).catch(() => {});
  }
  function watchResourceChanges(ref: ResourceRef, accept: Listener, _options?: { waitForBaseline?: boolean; graceMs?: number }) {
    if (disposed) throw new Error("The native subscription module is closed.");
    const entry = entryFor(ref);
    // Separate registrations can use the same callback object.
    const listener: Listener = (version) => accept(version);
    entry.changes.add(listener);
    start(entry);
    if (entry.version) listener(entry.version);
    let closed = false;
    return () => {
      if (closed) return;
      closed = true;
      entry.changes.delete(listener);
      if (!entry.changes.size) release(entry);
    };
  }
  return {
    watchResourceChanges,
    watchResourceConnection(accept: (status: ResourceConnectionState, stream: ResourceStreamState) => void) {
      connectionListeners.add(accept);
      accept(connection, stream);
      return () => { connectionListeners.delete(accept); };
    },
    getSnapshot<R extends ResourceRef>(ref: R): SubscriptionState<ResourceData<R>> {
      return entryFor(ref).state as SubscriptionState<ResourceData<R>>;
    },
    observe(ref: ResourceRef, accept: () => void) {
      const entry = entryFor(ref);
      entry.observers.add(accept);
      return () => { entry.observers.delete(accept); };
    },
    subscribe(ref: ResourceRef, accept: () => void, options?: { graceMs?: number }) {
      const entry = entryFor(ref);
      entry.observers.add(accept);
      const stop = watchResourceChanges(ref, () => {}, options);
      return () => { entry.observers.delete(accept); stop(); };
    },
    watchTokenRateEvents(accept: (value: ResourceTokenRatesEvent) => void) {
      if (disposed) throw new Error("The native subscription module is closed.");
      tokenListeners.add(accept);
      if (lastTokens) accept(lastTokens);
      if (tokenListeners.size === 1) {
        const generation = ++tokenGeneration;
        void track(context.then((native) => native.subscribe<ViewMessage>("subscriptions", { resources: [], tokenRates: true }, (event) => {
          if (disposed || generation !== tokenGeneration || !event.value) return;
          if (event.value.type === "token-rates") {
            lastTokens = event.value.value;
            for (const listener of tokenListeners) listener(lastTokens);
          } else if (event.value.type === "connection") updateConnection(event.value.status, event.value.stream);
        })).then((close) => {
          if (disposed || generation !== tokenGeneration) return closeView(close);
          else closeTokens = close;
        }).catch(() => { if (!disposed) updateConnection("degraded", "stopped"); }));
      }
      return () => {
        tokenListeners.delete(accept);
        if (!tokenListeners.size) {
          tokenGeneration++;
          const close = closeTokens;
          closeTokens = undefined;
          if (close) void closeView(close).catch(() => {});
        }
      };
    },
    async dispose() {
      if (disposed) return;
      disposed = true;
      for (const entry of entries.values()) { entry.changes.clear(); entry.observers.clear(); release(entry); }
      entries.clear();
      connectionListeners.clear();
      tokenListeners.clear();
      tokenGeneration++;
      if (closeTokens) void closeView(closeTokens).catch(() => {});
      closeTokens = undefined;
      while (pending.size) await Promise.allSettled([...pending]);
      if (closeErrors.length) throw new AggregateError(closeErrors, "The native subscription close result is unknown.");
    },
  };
}
