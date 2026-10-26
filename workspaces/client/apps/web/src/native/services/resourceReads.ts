import {
  subscriptionKey,
  type ResourceRef,
} from "../../sync/subscriptions/contracts";
import type { ResourceVersion } from "../../sync/resourceEvents";
import type { createNativeSubscriptions } from "../subscriptions";

type Resources = Pick<
  ReturnType<typeof createNativeSubscriptions>,
  "watchResourceChanges" | "watchResourceConnection"
>;

/** Native events trigger display reads. Retry clocks and transport stay native. */
export function createNativeResourceReads(resources: Resources) {
  return function watchResourceReads(
    selected: ResourceRef | readonly ResourceRef[],
    read: (version?: ResourceVersion) => Promise<void>,
    failed: (error: unknown) => void,
    options: { waitForBaseline?: boolean; reapplyInlineView?: boolean } = {},
  ) {
    let active = true;
    let running: Promise<void> | undefined;
    let dirty = false;
    let trigger: { key: string; version?: ResourceVersion } | undefined;
    const applied = new Map<string, ResourceVersion>();
    const same = (a?: ResourceVersion, b?: ResourceVersion) =>
      !!a && !!b && a.epoch === b.epoch && a.revision === b.revision;
    const refresh = (version?: ResourceVersion, key = "explicit") => {
      if (!active) return;
      const inline =
        options.reapplyInlineView && version?.viewSnapshot !== undefined;
      if (!inline && version && same(applied.get(key), version)) return;
      dirty = true;
      trigger = { key, version };
      if (running) return;
      running = Promise.resolve()
        .then(async () => {
          while (active && dirty) {
            dirty = false;
            const current = trigger;
            const inline =
              options.reapplyInlineView &&
              current?.version?.viewSnapshot !== undefined;
            if (
              !inline &&
              current?.version &&
              same(applied.get(current.key), current.version)
            )
              continue;
            try {
              await read(current?.version);
              if (active && current?.version)
                applied.set(current.key, current.version);
            } catch (error) {
              if (active) failed(error);
            }
          }
        })
        .finally(() => {
          running = undefined;
          if (active && dirty) refresh(trigger?.version, trigger?.key);
        });
    };
    const refs = Array.isArray(selected) ? selected : [selected];
    const stops = refs.map((ref) =>
      resources.watchResourceChanges(
        ref,
        (version) => {
          if (version) refresh(version, subscriptionKey(ref));
        },
        { waitForBaseline: options.waitForBaseline },
      ),
    );
    if (!options.waitForBaseline) refresh();
    const stop = () => {
      active = false;
      for (const close of stops) close();
    };
    return Object.assign(stop, { refresh: () => refresh() });
  };
}
