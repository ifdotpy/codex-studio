import { createContext, useContext, type ReactNode } from "react";
import type { StudioServer } from "./registry";
export type ServerSettings = {
  server?: StudioServer;
  panel: ReactNode;
  activate: () => void;
  close: () => void;
};
export const ServerSettingsContext = createContext<ServerSettings | null>(null);
export const useServerSettings = () => useContext(ServerSettingsContext);
