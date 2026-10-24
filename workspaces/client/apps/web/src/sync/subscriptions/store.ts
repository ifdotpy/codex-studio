import type {
  ResourceConnectionState,
  ResourceStreamState,
  ResourceVersion,
} from "../resourceEvents";
import {
  DEFAULT_SUBSCRIPTION_GRACE_MS,
  subscriptionKey,
  type ResourceData,
  type ResourceRef,
  type SubscriptionOptions,
  type SubscriptionState,
} from "./contracts";

export type SubscriptionTransport = {
  watchResourceChanges(
    ref: ResourceRef,
    accept: (version?: ResourceVersion) => void,
    options?: { waitForBaseline?: boolean },
  ): () => void;
  watchResourceConnection(
    accept: (
      status: ResourceConnectionState,
      stream: ResourceStreamState,
    ) => void,
  ): () => void;
};

type ChangeListener = (version?: ResourceVersion) => void;
type Entry = {
  ref: ResourceRef;
  consumers: Set<ChangeListener>;
  observers: Set<() => void>;
  state: SubscriptionState<unknown>;
  version?: ResourceVersion;
  confirmed: boolean;
  stop?: () => void;
  timer?: ReturnType<typeof setTimeout>;
  graceMs: number;
};

/** The transport retains peer aggregation; this store owns local screen lifetime.
 * A zero-consumer entry stays subscribed through grace, but its cache is stale. */
export function createSubscriptionStore(transport: SubscriptionTransport) {
  const entries = new Map<string, Entry>();
  let connection: ResourceConnectionState = "connecting";
  let stopConnection: (() => void) | undefined;
  let disposed = false;

  const entryFor = (ref: ResourceRef) => {
    const key = subscriptionKey(ref);
    let entry = entries.get(key);
    if (!entry) {
      entry = {
        ref,
        consumers: new Set(),
        observers: new Set(),
        state: { data: undefined, status: "loading", error: null },
        confirmed: false,
        graceMs: DEFAULT_SUBSCRIPTION_GRACE_MS,
      };
      entries.set(key, entry);
    }
    return entry;
  };
  const notify = (entry: Entry) => {
    for (const accept of entry.observers) accept();
  };
  const status = (entry: Entry) => {
    const next =
      entry.state.error !== null
        ? "error"
        : !entry.consumers.size || connection !== "live"
          ? "stale"
          : entry.confirmed
            ? "live"
            : entry.state.data === undefined
              ? "loading"
              : "stale";
    if (entry.state.status !== next) {
      entry.state = { ...entry.state, status: next };
      notify(entry);
    }
  };
  const ensureConnection = () => {
    if (stopConnection) return;
    stopConnection = transport.watchResourceConnection((next) => {
      connection = next;
      for (const entry of entries.values()) {
        if (next !== "live") entry.confirmed = false;
        status(entry);
      }
    });
  };
  const releaseSource = (entry: Entry) => {
    clearTimeout(entry.timer);
    entry.timer = undefined;
    entry.stop?.();
    entry.stop = undefined;
    // Retain domain data, but a future new interest needs an authoritative frame.
    entry.version = undefined;
    entry.confirmed = false;
    status(entry);
    if (![...entries.values()].some((candidate) => candidate.stop)) {
      stopConnection?.();
      stopConnection = undefined;
    }
  };
  const watchResourceChanges = (
    ref: ResourceRef,
    accept: ChangeListener,
    options: SubscriptionOptions & { waitForBaseline?: boolean } = {},
  ) => {
    if (disposed) throw new Error("The subscription module is closed.");
    const graceMs = options.graceMs ?? DEFAULT_SUBSCRIPTION_GRACE_MS;
    if (!Number.isFinite(graceMs) || graceMs < 0)
      throw new RangeError(
        "Subscription graceMs must be finite and nonnegative.",
      );
    const entry = entryFor(ref);
    clearTimeout(entry.timer);
    entry.timer = undefined;
    // Independent registrations may use the same callback object.
    const listener: ChangeListener = (version) => accept(version);
    const starting = !entry.stop;
    entry.graceMs = entry.consumers.size
      ? Math.max(entry.graceMs, graceMs)
      : graceMs;
    entry.consumers.add(listener);
    ensureConnection();
    if (starting) {
      let replayingCache = true;
      entry.stop = transport.watchResourceChanges(
        ref,
        (version) => {
          entry.version = version;
          if (!replayingCache)
            entry.confirmed =
              !!version &&
              ((!!version.cursor && !!version.replyKind) ||
                !!version.progress ||
                !!version.discoverySnapshot ||
                !!version.voiceRecords ||
                !!version.draftChanges);
          entry.state = { ...entry.state, error: null };
          status(entry);
          for (const callback of entry.consumers) callback(version);
        },
        { waitForBaseline: options.waitForBaseline },
      );
      replayingCache = false;
    } else if (entry.version) listener(entry.version);
    status(entry);
    let stopped = false;
    return () => {
      if (stopped) return;
      stopped = true;
      entry.consumers.delete(listener);
      status(entry);
      if (!entry.consumers.size) {
        if (entry.graceMs === 0) releaseSource(entry);
        else
          entry.timer = setTimeout(() => releaseSource(entry), entry.graceMs);
      }
    };
  };

  return {
    watchResourceChanges,
    getSnapshot<R extends ResourceRef>(
      ref: R,
    ): SubscriptionState<ResourceData<R>> {
      return entryFor(ref).state as SubscriptionState<ResourceData<R>>;
    },
    observe(ref: ResourceRef, accept: () => void) {
      const entry = entryFor(ref);
      entry.observers.add(accept);
      return () => {
        entry.observers.delete(accept);
      };
    },
    publish<R extends ResourceRef>(ref: R, data: ResourceData<R>) {
      const entry = entryFor(ref);
      entry.state = { ...entry.state, data, error: null };
      status(entry);
      notify(entry);
    },
    fail(ref: ResourceRef, error: unknown) {
      const entry = entryFor(ref);
      entry.state = {
        ...entry.state,
        status: "error",
        error: error instanceof Error ? error : new Error(String(error)),
      };
      notify(entry);
    },
    dispose() {
      disposed = true;
      for (const entry of entries.values()) {
        entry.consumers.clear();
        releaseSource(entry);
        entry.observers.clear();
      }
      entries.clear();
      stopConnection?.();
      stopConnection = undefined;
    },
  };
}
export type SubscriptionStore = ReturnType<typeof createSubscriptionStore>;
