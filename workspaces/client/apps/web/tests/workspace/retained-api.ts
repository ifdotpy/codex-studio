import type { ApiGetPath, GetResult } from "../../src/apiTypes.ts";
import type { GetOptions } from "../../src/api.ts";
async function get<Path extends ApiGetPath>(
  path: Path,
  options: GetOptions<Path>,
): Promise<GetResult<Path>> {
  const response = await fetch(
    path + "?" + new URLSearchParams(options.query as Record<string, string>),
  );
  return response.json();
}
import type { watchResourceReads } from "../../src/sync/subscriptions/reads.ts";
declare global {
  interface Window {
    refreshRetained: () => Promise<void>;
    openedFolder: string;
  }
}
const api = { get };
export const useServerApi = () => api;
// This one-off watcher fixture omits the unused refresh method on its disposer.
const watch = ((_resource, read, failed) => {
  let active = true;
  window.refreshRetained = async () => {
    if (active) await read().catch(failed);
  };
  queueMicrotask(() => {
    if (active) void window.refreshRetained();
  });
  return () => {
    active = false;
  };
}) as typeof watchResourceReads;
export const useServerResourceReads = () => watch;
export const useServerFileDialogs = () => ({
  canRevealPath: () => true,
  revealPath: async (path: string) => {
    window.openedFolder = path;
  },
});
