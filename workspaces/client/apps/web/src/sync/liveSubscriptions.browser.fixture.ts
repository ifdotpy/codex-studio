declare global {
  interface Window {
    syncSubscriptions: {
      transport: ReturnType<
        typeof import("./resourceEvents").createResourceEvents
      >;
    };
  }
}
export const initializeSubscriptions = async (workspaceId: string) => {
  const { createResourceEvents } = await import("/src/sync/resourceEvents.ts");
  const api = await import("/src/api.ts");
  const { openServerEvents } = await import("/src/servers/eventSource.ts");
  const { getWorkspaceTokenRates } = await import("/src/usage/tokenRate.ts");
  api.setToken("fixture-token");
  const transport = createResourceEvents({
    api,
    sync: {
      syncDatabase: async () => ({ workspaceId }),
      persistResourceEntityChanges: async () => {},
    },
    tokenRates: {
      configureTokenRateStream() {},
      getWorkspaceTokenRates,
      receiveResourceTokenRates() {
        return true;
      },
    },
    openEvents: openServerEvents,
    storageName: (name) => name,
  });
  transport.watchResourceChanges({ kind: "state" }, () => {});
  let stop: (() => void) | undefined;
  const select = (agentId: string) => {
    stop?.();
    stop = transport.watchResourceChanges(
      { kind: "transcript", agentId },
      () => {
        const output = document.querySelector("output");
        if (!output) throw new Error("Missing subscription output");
        output.textContent = agentId;
      },
    );
  };
  const one = document.querySelector<HTMLButtonElement>("#one");
  const two = document.querySelector<HTMLButtonElement>("#two");
  if (!one || !two) throw new Error("Missing subscription buttons");
  one.onclick = () => select("one");
  two.onclick = () => select("two");
  window.syncSubscriptions = { transport };
  select("one");
};
