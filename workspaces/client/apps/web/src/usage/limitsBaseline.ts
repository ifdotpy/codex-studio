import type { ResourceVersion } from "../sync/resourceEvents";
import { limitsHydrationSucceeded } from "./accountUsage";
import type { AccountLimitsSnapshot } from "./accountUsage";

export type LimitsBaselineHydration =
  | boolean
  | { snapshot: AccountLimitsSnapshot };

function alreadyHandled(
  handled: ResourceVersion | undefined,
  incoming: ResourceVersion | undefined,
) {
  return !!(
    handled &&
    incoming &&
    handled.epoch === incoming.epoch &&
    incoming.revision <= handled.revision
  );
}

function isNewerVersion(
  incoming: ResourceVersion,
  current: ResourceVersion | undefined,
) {
  return (
    !current ||
    incoming.epoch !== current.epoch ||
    incoming.revision > current.revision
  );
}

/**
 * State is `latestSeen`, `handled`, one `running` promise, and `baselinePending` for versionless resets.
 * Every versioned notification updates `latestSeen` synchronously before joining or starting the drain.
 * Each turn captures a target and checks activity; fresh cache advances handled, while other turns await hydration/read.
 * Hydration covers only the first baseline when its matching snapshot is fresh (<60 s), error-free, and has data.
 * A successful turn handles only its captured target; null-data reads are successful and replace stale data.
 * The loop rechecks newer versions; failure stops unless a newer version arrived, and stale results cannot spin.
 */
export function limitsBaselineReader(
  hydrate: () => Promise<LimitsBaselineHydration>,
  read: () => Promise<void | boolean>,
  isActive: () => boolean = () => true,
  isSnapshotFresh: (snapshot?: AccountLimitsSnapshot) => boolean = () => true,
): (version?: ResourceVersion) => Promise<void> {
  let baselinePending = true;
  let latestSeen: ResourceVersion | undefined;
  let handledVersion: ResourceVersion | undefined;
  let hydration: Promise<LimitsBaselineHydration> | undefined;
  let running: Promise<void> | undefined;

  const hasWork = () =>
    baselinePending ||
    (!!latestSeen && isNewerVersion(latestSeen, handledVersion));

  const startRun = (): Promise<void> => {
    if (running) return running;
    let resolveOperation!: () => void;
    let rejectOperation!: (reason: unknown) => void;
    const operation = new Promise<void>((resolve, reject) => {
      resolveOperation = resolve;
      rejectOperation = reject;
    });
    running = operation;
    const run = async () => {
      try {
        while (hasWork()) {
          if (!isActive()) return;

          const target = latestSeen;
          const isBaseline = baselinePending;
          baselinePending = false;
          const newerVersionArrived = () =>
            !!latestSeen && (!target || isNewerVersion(latestSeen, target));
          try {
            if (!isBaseline && target && !handledVersion && isSnapshotFresh()) {
              handledVersion = target;
              continue;
            }

            if (isBaseline && !handledVersion) {
              hydration ??= hydrate().catch(() => false);
              const hydrationResult = await hydration;
              const hydratedSnapshot =
                typeof hydrationResult === "object"
                  ? hydrationResult.snapshot
                  : undefined;
              const hydrationFresh = isSnapshotFresh(hydratedSnapshot);
              const hydrationUsable =
                typeof hydrationResult !== "object" ||
                limitsHydrationSucceeded(hydratedSnapshot ?? null);
              if (hydrationResult && hydrationFresh && hydrationUsable) {
                if (target && isNewerVersion(target, handledVersion))
                  handledVersion = target;
                if (!target && !latestSeen) {
                  baselinePending = true;
                  return;
                }
                continue;
              }
              if (!isActive()) {
                baselinePending = true;
                return;
              }
            }

            const readSucceeded = (await read()) !== false;
            if (readSucceeded) {
              if (target && isNewerVersion(target, handledVersion))
                handledVersion = target;
              continue;
            }

            if (isBaseline) baselinePending = true;
            if (newerVersionArrived()) continue;
            return;
          } catch (error) {
            if (isBaseline) baselinePending = true;
            if (newerVersionArrived()) continue;
            throw error;
          }
        }
      } finally {
        if (running === operation) running = undefined;
      }
    };
    void run().then(resolveOperation, rejectOperation);
    return operation;
  };

  return (version) => {
    if (version) {
      if (isNewerVersion(version, latestSeen)) latestSeen = version;
      if (alreadyHandled(handledVersion, version) && !isSnapshotFresh())
        baselinePending = true;
    } else {
      baselinePending = true;
    }

    if (running) return running;
    if (!hasWork()) return Promise.resolve();
    return startRun();
  };
}
