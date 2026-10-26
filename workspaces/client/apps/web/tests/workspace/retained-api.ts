import { get } from "./fixture-api.ts";
import type { watchResourceReads } from "../../src/sync/subscriptions/reads.ts";
declare global {
  interface Window {
    retainedWorktreesRefresh: () => Promise<void>;
    retainedWorktreesOpenedFolder: string;
  }
}
const api = { get };
export const useServerApi = () => api;
const watch: typeof watchResourceReads = (_resource, read, failed) => {
  let active = true;
  window.retainedWorktreesRefresh = async () => {
    if (active) await read().catch(failed);
  };
  queueMicrotask(() => {
    if (active) void window.retainedWorktreesRefresh();
  });
  return Object.assign(
    () => {
      active = false;
    },
    {
      refresh: () => {
        void window.retainedWorktreesRefresh();
      },
    },
  );
};
export const useServerResourceReads = () => watch;
export const useServerFileDialogs = () => ({
  canRevealPath: () => true,
  revealPath: async (path: string) => {
    window.retainedWorktreesOpenedFolder = path;
  },
});
