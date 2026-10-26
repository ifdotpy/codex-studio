import type { components } from "../../generated/api.ts";

export type ResourceVersion = {
  viewSnapshot?: components["schemas"]["SubscriptionViewSnapshot"] | undefined;
  cursor?: components["schemas"]["SubscriptionCursor"] | undefined;
  replyKind?: components["schemas"]["SubscriptionReplyKind"] | undefined;
  /** Transient subscribed value; never sent through the entity persister. */
  progress?: ResourceProgress;
  epoch: string;
  revision: number;
  reason?: components["schemas"]["ResourceChangeEvent"]["reason"];
  draftChanges?: components["schemas"]["DraftChangeBatch"] | undefined;
  /** Transient access metadata; never written to the entity cache. */
  discoverySnapshot?: components["schemas"]["AccessSnapshot"] | undefined;
  /** Transient cursor interval and media session state. */
  voiceRecords?: components["schemas"]["VoiceRecordBatch"] | undefined;
  /** Highest entity row represented by this StateResource revision, when present. */
  entitySequence?: number;
  /** Transcript cursor persisted before this notification; readers verify local coverage. */
  transcriptSequence?: number;
  /** A revision-only transcript change is the server's oversized-interval signal. */
  transcriptReadRequired?: boolean;
  /** The server cannot enumerate the exact changed rows; clients must reconcile. */
  entitySequenceReset?: boolean;
};

export type ResourceProgressEvent =
  components["schemas"]["ResourceProgressEvent"];
export type ResourceProgress = ResourceProgressEvent extends infer Progress
  ? Progress extends { resource: infer Resource; value: infer Value }
    ? { resource: Resource; value: Value }
    : never
  : never;

export type ResourceRef = components["schemas"]["ResourceRef"];
/** The OpenAPI document owns this mapping; contracts:generate emits its types. */
export type ResourceData<R extends ResourceRef> =
  components["schemas"]["SubscriptionDataMap"][R["kind"]];

export type SubscriptionStatus = "loading" | "live" | "stale" | "error";
export type SubscriptionState<T> = {
  data: T | undefined;
  status: SubscriptionStatus;
  error: Error | null;
};
export type SubscriptionOptions = { graceMs?: number | undefined };
/** Retain interest after the final consumer leaves, in milliseconds. */
export const DEFAULT_SUBSCRIPTION_GRACE_MS = 20_000;
/** Maximum inactive domain snapshots retained by one server module, in entries. */
export const MAX_INACTIVE_SUBSCRIPTION_CACHE_ENTRIES = 128;

/** Durable receipts or inline transient values prove this ref was applied.
 * Revision-only hints never establish authoritative resource readiness. */
export function hasAppliedResourceBaseline(
  version?: ResourceVersion,
  appliedView?: ResourceRef,
): boolean {
  // Cursorless views are authoritative only after the matching data adapter
  // applies them. Durable room/terminal payloads still require their receipt.
  const view = version?.viewSnapshot;
  const inlineViewApplied =
    (appliedView?.kind === "processes" && view?.processes !== undefined) ||
    (appliedView?.kind === "complaint" && view?.complaint !== undefined) ||
    (appliedView?.kind === "room" &&
      appliedView.legacy === true &&
      view?.legacyMessages !== undefined);
  return (
    !!version &&
    ((!!version.cursor && !!version.replyKind) ||
      !!version.progress ||
      !!version.discoverySnapshot ||
      !!version.voiceRecords ||
      !!version.draftChanges ||
      inlineViewApplied)
  );
}

/** Stable identity for subscription ownership and durable cursor keys. Never decode it. */
export function subscriptionKey(ref: ResourceRef): string {
  return JSON.stringify(
    Object.entries(ref)
      .filter(([, value]) => value !== undefined)
      .sort(([left], [right]) => left.localeCompare(right)),
  );
}

export type UseSubscription = <R extends ResourceRef>(
  ref: R | null,
  options?: SubscriptionOptions,
) => SubscriptionState<ResourceData<R>>;
