import type {
  ResourceConnectionState,
  ResourceProgress,
  ResourceRef,
  ResourceVersion,
} from "./resourceEvents";

type ProgressResource = Extract<
  ResourceRef,
  { kind: "auth-progress" | "vault-readiness" }
>;
type ProgressEvents = {
  watchResourceChanges: (
    resource: ResourceRef,
    accept: (version?: ResourceVersion) => void,
  ) => () => void;
  watchResourceConnection: (
    accept: (state: ResourceConnectionState) => void,
  ) => () => void;
};

/** Subscribe through the existing stream. HTTP is reserved for recovery or an
 * explicit refresh; ordinary resource hints never cause a read. */
export function subscribeProgress(
  events: ProgressEvents,
  resource: ProgressResource,
  accept: (progress: ResourceProgress) => void,
  recover: () => Promise<unknown>,
  failed: (error: unknown) => void,
) {
  let active = true;
  let recovering = false;
  let interrupted = false;
  let missingRecovered = false;
  const refresh = () => {
    if (!active || recovering) return;
    recovering = true;
    void recover()
      .catch((error) => {
        if (active) failed(error);
      })
      .finally(() => {
        recovering = false;
      });
  };
  const stopChanges = events.watchResourceChanges(resource, (version) => {
    if (!active || !version?.progress) return;
    const progress = version.progress;
    if (progress.resource.kind !== resource.kind) return;
    if (progress.value === null) {
      if (!missingRecovered) {
        missingRecovered = true;
        refresh();
      }
    } else accept(progress);
  });
  const stopConnection = events.watchResourceConnection((state) => {
    if (state === "degraded" || state === "offline") interrupted = true;
    if (state === "live" && interrupted) {
      interrupted = false;
      refresh();
    }
  });
  const stop = () => {
    active = false;
    stopChanges();
    stopConnection();
  };
  stop.refresh = refresh;
  return stop;
}
