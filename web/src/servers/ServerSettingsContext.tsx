import { createContext, useContext, type ReactNode } from "react";
export type ServerSettings = {
  panel: ReactNode;
  activate: () => void;
  close: () => void;
};
export const ServerSettingsContext = createContext<ServerSettings | null>(null);
export const useServerSettings = () => useContext(ServerSettingsContext);
