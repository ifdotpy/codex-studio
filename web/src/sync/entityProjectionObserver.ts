import type { RxCollection } from "rxdb";
import type { Snapshot } from "../types";
import { createFrameBatch } from "./frameBatch";
import { readRetryDelay, retryableReadError } from "./readRetry";
import {
  applyEntityChanges,
  emptyEntityProjection,
  type EntityRow,
} from "./entityProjection";

/** Read one baseline, then consume changed documents without a live find query. */
export async function observeEntityProjection(
  collection: RxCollection<EntityRow>,
  accept: (snapshot: Snapshot) => void,
  fail: (error: unknown) => void,
) {
  let projection = emptyEntityProjection();
  let stopped = false;
  let ready = false;
  let loaded = false;
  let markerObserved = false;
  let markerSequence = -1;
  let generation = 0;
  let retryCount = 0;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let reading = false;
  let markerDirty = false;
  const pendingIds = new Set<string>();
  let published: Snapshot | null = null;
  const publication = createFrameBatch(() => void publishCurrent());
  const publishCurrent = async () => {
    if (stopped || reading) return;
    const current = generation;
    const ids = ready && loaded ? [...pendingIds] : [];
    for (const id of ids) pendingIds.delete(id);
    reading = true;
    markerDirty = false;
    let failed = false;
    try {
      // BroadcastChannel can deliver old events after a reset. Events only mark
      // IDs dirty; the current durable row and marker determine what to show.
      const documents = await collection.storageInstance.findDocumentsById(
        ["state:entities:ready", ...ids],
        true,
      );
      if (stopped || current !== generation) return;
      if (markerDirty) {
        for (const id of ids) pendingIds.add(id);
        return;
      }
      updateMarker(documents.find((row) => row.id === "state:entities:ready"));
      if (current !== generation || !ready || !loaded) return;
      const next = applyEntityChanges(projection, documents, true);
      retryCount = 0;
      if (next && next !== published) {
        published = next;
        accept(next);
      }
    } catch (error) {
      if (stopped || current !== generation) return;
      failed = true;
      for (const id of ids) pendingIds.add(id);
      fail(error);
      if (retryableReadError(error))
        retryTimer = setTimeout(() => {
          retryTimer = undefined;
          publication.schedule();
        }, readRetryDelay(retryCount++));
    } finally {
      reading = false;
      if (
        !stopped &&
        !failed &&
        retryTimer === undefined &&
        (markerDirty ||
          (ready &&
            loaded &&
            (pendingIds.size > 0 || projection.snapshot === null)))
      )
        publication.schedule();
    }
  };
  const load = async () => {
    const current = ++generation;
    try {
      const documents = await collection
        .find({ selector: { id: { $gte: "entity:", $lt: "entity;" } } })
        .exec();
      if (stopped || current !== generation || !ready) return;
      applyEntityChanges(
        projection,
        documents.map((document) => document.toJSON()),
        false,
      );
      loaded = true;
      retryCount = 0;
      publication.schedule();
    } catch (error) {
      if (stopped || current !== generation) return;
      fail(error);
      if (retryableReadError(error))
        retryTimer = setTimeout(() => {
          retryTimer = undefined;
          void load();
        }, readRetryDelay(retryCount++));
    }
  };
  const updateMarker = (row?: EntityRow) => {
    if (row && row.seq < markerSequence) return;
    const advanced = !!row && markerSequence >= 0 && row.seq > markerSequence;
    if (row) markerSequence = row.seq;
    if (row?.payload !== "ready" || row._deleted || advanced) {
      // Reset can lower every server sequence. Discard the prior epoch and read
      // a complete replacement baseline only after its ready marker arrives.
      generation++;
      clearTimeout(retryTimer);
      retryTimer = undefined;
      retryCount = 0;
      ready = false;
      loaded = false;
      projection = emptyEntityProjection();
      pendingIds.clear();
      publication.cancel();
    }
    if (row?.payload === "ready" && !row._deleted && !ready) {
      ready = true;
      void load();
    }
  };
  const subscription = collection.$.subscribe((event) => {
    if (stopped) return;
    const row = event.documentData;
    if (row.id === "state:entities:ready") {
      markerObserved = true;
      markerDirty = true;
      publication.schedule();
    } else if (ready && row.id.startsWith("entity:")) {
      pendingIds.add(row.id);
      if (loaded) publication.schedule();
    }
  });
  const stop = () => {
    stopped = true;
    generation++;
    clearTimeout(retryTimer);
    subscription.unsubscribe();
    publication.cancel();
    pendingIds.clear();
  };
  try {
    // Subscribe first, then read the marker before the baseline. A reset already
    // in progress must not expose or retain rows from the previous epoch.
    const [marker] = await collection.storageInstance.findDocumentsById(
      ["state:entities:ready"],
      true,
    );
    if (!markerObserved) updateMarker(marker);
    return stop;
  } catch (error) {
    stop();
    throw error;
  }
}
