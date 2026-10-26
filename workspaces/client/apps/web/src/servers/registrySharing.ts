import type { StudioServer } from "./registry";

/** Keep each unchanged server object, and the list itself when nothing changed. A reread
 * of the same saved list must not look like a new server to open dialogs and connections. */
export function shareServers(
  previous: readonly StudioServer[],
  next: StudioServer[],
): StudioServer[] {
  const prior = new Map(previous.map((server) => [server.id, server]));
  const shared = next.map((server) => {
    const old = prior.get(server.id);
    return old && JSON.stringify(old) === JSON.stringify(server) ? old : server;
  });
  return shared.length === previous.length &&
    shared.every((server, index) => server === previous[index])
    ? (previous as StudioServer[])
    : shared;
}
