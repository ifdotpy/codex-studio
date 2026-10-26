import { useState } from "react";
import { ServerServicesProvider } from "./ServerServicesContext";
import { createServerServices } from "./serverServices";
import {
  localServer,
  readServers,
  writeServers,
  SERVER_REGISTRY_EVENT,
  SERVER_REGISTRY_KEY,
} from "./registry";
import { orderPreferenceServers } from "./preferenceServerOrder";
import { writePreferenceEdit } from "../sync/uiPreferenceStore";
import { fetchServerSummary } from "./summary";
import { serverSidebarBackend } from "./serverSidebarBackend";
import MultiServerShell from "./MultiServerShell";
import type { MultiServerRuntime } from "./multiServerRuntime";
import type { ServerServices } from "./serverServices";

const uiOnly =
  new URLSearchParams(location.search).get("studio-ui-only") === "1";
export default function MultiServerApp() {
  const [localServices] = useState(() =>
    uiOnly ? undefined : createServerServices(localServer()),
  );
  const [runtime] = useState<MultiServerRuntime<ServerServices>>(() => ({
    uiOnly,
    storage: globalThis.localStorage,
    localServices,
    registry: {
      read: readServers,
      local: localServer,
      write: writeServers,
      order: orderPreferenceServers,
      subscribe(changed) {
        const stored = (event: StorageEvent) => {
          if (event.key === SERVER_REGISTRY_KEY || event.key === null)
            changed();
        };
        window.addEventListener("storage", stored);
        window.addEventListener(SERVER_REGISTRY_EVENT, changed);
        return () => {
          window.removeEventListener("storage", stored);
          window.removeEventListener(SERVER_REGISTRY_EVENT, changed);
        };
      },
    },
    openService: createServerServices,
    provideServices: (service, children) => (
      <ServerServicesProvider services={service}>
        {children}
      </ServerServicesProvider>
    ),
    sidebarBackend: serverSidebarBackend,
    accountServices: (service) => ({
      watchAccounts: (receive, failed) =>
        service.sync.subscribeStateProjection(
          (snapshot) => receive(snapshot?.runtime.accountsSummary?.accounts),
          failed,
        ),
      renderScope: (children) => (
        <ServerServicesProvider services={service}>
          {children}
        </ServerServicesProvider>
      ),
    }),
    summary: fetchServerSummary,
    editPreference: writePreferenceEdit,
    subscribePreferences(changed) {
      window.addEventListener("storage", changed);
      window.addEventListener("studio-preferences-change", changed);
      return () => {
        window.removeEventListener("storage", changed);
        window.removeEventListener("studio-preferences-change", changed);
      };
    },
  }));
  const shell = <MultiServerShell runtime={runtime} />;
  return localServices ? (
    <ServerServicesProvider services={localServices}>
      {shell}
    </ServerServicesProvider>
  ) : (
    shell
  );
}
