import { createContext, useContext, type ReactNode } from "react";
import type { StudioServer } from "./registry";
import type { StudioSettingsTab } from "../components/StudioSettingsTabs";
export type ServerSettings = {
  server?: StudioServer;
  panel: ReactNode;
  settingsOpen: boolean;
  settingsTab: StudioSettingsTab;
  setSettingsOpen: (opened: boolean) => void;
  setSettingsTab: (tab: StudioSettingsTab) => void;
};
export const ServerSettingsContext = createContext<ServerSettings | null>(null);
export const useServerSettings = () => useContext(ServerSettingsContext);
