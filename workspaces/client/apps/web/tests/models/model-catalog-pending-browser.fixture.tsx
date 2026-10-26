import React from "react";
import { createRoot } from "react-dom/client";
import type { Root } from "react-dom/client";
import { flushSync } from "react-dom";
import {
  ModelCatalogReader,
  useWorkerModels,
} from "../../src/components/agents/WorkerModelPicker.tsx";
import { get } from "../../src/api.ts";

type Reader = React.ContextType<typeof ModelCatalogReader>;
export type ModelSnapshot = Parameters<
  Parameters<Reader["subscribeState"]>[0]
>[0];
declare global {
  interface Window {
    unmountCatalog: () => void;
    modelSnapshot: ModelSnapshot;
    modelGets: number;
    acceptModelSnapshot:
      | ((snapshot: ModelSnapshot) => void)
      | undefined
      | undefined;
    catalogResult: ReturnType<typeof useWorkerModels> | undefined;
    catalogRoot: Root | undefined | undefined;
    mountCatalog: (account?: string, workers?: boolean) => void;
    publishModelSnapshot: (snapshot: ModelSnapshot) => void;
  }
}

const reader: Reader = {
  scope: "fixture",
  cacheOwner: {},
  read: (options) => get("/api/models", options),
  subscribeState: (accept) => {
    window.acceptModelSnapshot = accept;
    accept(window.modelSnapshot);
    return () => {
      window.acceptModelSnapshot = undefined;
    };
  },
};
function Fixture({ account, workers }: { account: string; workers: boolean }) {
  const result = useWorkerModels(account, true, workers);
  window.catalogResult = result;
  return <pre>{JSON.stringify(result)}</pre>;
}
window.mountCatalog = (account = "default", workers = false) => {
  const element = document.getElementById("app")!;
  const root = (window.catalogRoot ||= createRoot(element));
  flushSync(() =>
    root.render(
      <ModelCatalogReader.Provider value={reader}>
        <Fixture account={account} workers={workers} />
      </ModelCatalogReader.Provider>,
    ),
  );
};
window.publishModelSnapshot = (snapshot) => {
  window.modelSnapshot = snapshot;
  window.acceptModelSnapshot?.(snapshot);
};

window.unmountCatalog = () => {
  flushSync(() => window.catalogRoot?.unmount());
  window.catalogRoot = undefined;
};
