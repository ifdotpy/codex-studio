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
    modelCatalogFixture: {
      snapshot: ModelSnapshot;
      gets: number;
      accept?: ((snapshot: ModelSnapshot) => void) | undefined;
      result?: ReturnType<typeof useWorkerModels>;
      root?: Root | undefined;
      mount: (account?: string, workers?: boolean) => void;
      publish: (snapshot: ModelSnapshot) => void;
    };
  }
}
const harness = window.modelCatalogFixture;
const reader: Reader = {
  scope: "fixture",
  cacheOwner: {},
  read: (options) => get("/api/models", options),
  subscribeState: (accept) => {
    harness.accept = accept;
    accept(harness.snapshot);
    return () => {
      harness.accept = undefined;
    };
  },
};
function Fixture({ account, workers }: { account: string; workers: boolean }) {
  const result = useWorkerModels(account, true, workers);
  harness.result = result;
  return <pre>{JSON.stringify(result)}</pre>;
}
harness.mount = (account = "default", workers = false) => {
  const element = document.getElementById("app");
  if (!element) throw new Error("Missing catalog root");
  const root = (harness.root ??= createRoot(element));
  flushSync(() =>
    root.render(
      <ModelCatalogReader.Provider value={reader}>
        <Fixture account={account} workers={workers} />
      </ModelCatalogReader.Provider>,
    ),
  );
};
harness.publish = (snapshot) => {
  harness.snapshot = snapshot;
  harness.accept?.(snapshot);
};
