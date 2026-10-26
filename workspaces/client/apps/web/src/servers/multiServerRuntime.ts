import type { ReactNode } from "react";
import type { ServerApi } from "../api";
import type { SidebarBackend } from "../components/sidebar/services";
import type { StudioServer } from "./registry";
import type { AccountServices } from "./accountServices";
import type { AppPreferences } from "./appPreferences";
import type { fetchServerSummary } from "./summary";
import type { ShellServiceInstance } from "./shellServicePool";

export interface ShellServices extends ShellServiceInstance {
  api: Pick<ServerApi, "get">;
}
/** One mount supplies these ports. No selected-server or global service lookup. */
export interface MultiServerRuntime<S extends ShellServices> {
  uiOnly: boolean;
  storage: Storage;
  localServices?: S;
  registry: {
    read(): StudioServer[];
    local(): StudioServer;
    write(servers: StudioServer[]): void | Promise<void>;
    subscribe(changed: () => void): () => void;
    order(servers: StudioServer[]): StudioServer[];
  };
  /** An async native open is admitted only after its ready() resolves. */
  openService(server: StudioServer): S | Promise<S>;
  provideServices(service: S, children: ReactNode): ReactNode;
  sidebarBackend(service: S, online: () => boolean): SidebarBackend;
  accountServices(service: S): AccountServices;
  summary: typeof fetchServerSummary;
  editPreference: AppPreferences["editPreference"];
  subscribePreferences(changed: () => void): () => void;
}
