import type { components } from "../generated/api";
import { watchResourceChanges } from "../sync/resourceEvents";

type ResourceRef = components["schemas"]["ResourceRef"];

/**
 * Run one targeted read for the initial resource baseline and each later
 * notification. If a notification arrives during a read, perform one more
 * read after it settles so the latest version cannot be lost.
 */
export function watchResourceReads(
  resources: ResourceRef | readonly ResourceRef[],
  read: () => Promise<void>,
  failed: (error: unknown) => void,
): () => void {
  const refs = Array.isArray(resources) ? resources : [resources];
  let active = true;
  let reading = false;
  let dirty = false;
  let scheduled = false;

  const refresh = () => {
    dirty = true;
    if (!active || reading || scheduled) return;
    scheduled = true;
    queueMicrotask(() => {
      scheduled = false;
      if (!active || reading || !dirty) return;
      reading = true;
      void (async () => {
        while (active && dirty) {
          dirty = false;
          try {
            await read();
          } catch (error) {
            if (active) failed(error);
          }
        }
        reading = false;
      })();
    });
  };

  const stops = refs.map((resource) => watchResourceChanges(resource, refresh));
  return () => {
    active = false;
    dirty = false;
    for (const stop of stops) stop();
  };
}
