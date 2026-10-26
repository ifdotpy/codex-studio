import type {
  ResourceRef,
  watchResourceChanges as actualWatch,
  watchResourceConnection as actualConnection,
} from "../../src/sync/resourceEvents.ts";
import type {} from "./native-runtime-status-browser.fixture.tsx";
type ChangeListener = Parameters<typeof actualWatch>[1];
type ConnectionListener = Parameters<typeof actualConnection>[0];
const subscribers = new Map<ResourceRef["kind"], Set<ChangeListener>>();
// Assign after the page module initializes its harness.
export function emitRuntimeChange(kind: ResourceRef["kind"]) {
  for (const callback of subscribers.get(kind) || [])
    callback({ epoch: "fixture", revision: Date.now() });
}
export function watchResourceChanges(
  resource: ResourceRef,
  callback: ChangeListener,
) {
  let group = subscribers.get(resource.kind);
  if (!group) subscribers.set(resource.kind, (group = new Set()));
  group.add(callback);
  callback({ epoch: "fixture", revision: 0 });
  return () => group.delete(callback);
}
export function watchResourceConnection(
  listener: (
    status: Parameters<ConnectionListener>[0],
    stream: Parameters<ConnectionListener>[1],
  ) => void,
) {
  listener("live", "open");
  return () => {};
}
