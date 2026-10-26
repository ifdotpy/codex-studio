import { get, post } from "./fixture-api.ts";
const api = {
  get,
  post,
  apiDownload: async () => ({
    blob: new Blob(["failed check"]),
    name: "gate.log",
    truncated: false,
  }),
};
export const useServerApi = () => api;
export const useServerStorage = () => localStorage;
