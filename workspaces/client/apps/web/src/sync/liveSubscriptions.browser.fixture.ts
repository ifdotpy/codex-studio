declare global {
  interface Window {
    transport: ReturnType<
      typeof import("./resourceEvents").createResourceEvents
    >;
  }
}
export const initializeSubscriptions = async (workspaceId: string) => {
  await import("/src/sync/client.ts");
  const { createResourceEvents } = await import("/src/sync/resourceEvents.ts");
  const api = await import("/src/api.ts");
  const { openServerEvents } = await import("/src/servers/eventSource.ts");
  api.setToken("fixture-token");
  // This subscription-only fixture omits getWorkspaceTokenRates; it never reads token rates.
  const tokenRates: Partial<
    Parameters<typeof createResourceEvents>[0]["tokenRates"]
  > = {
    configureTokenRateStream() {},
    receiveResourceTokenRates() {
      return true;
    },
  };
  const transport = createResourceEvents({
    api,
    sync: {
      syncDatabase: async () => ({ workspaceId }),
      persistResourceEntityChanges: async () => {},
    },
    tokenRates: tokenRates as Parameters<
      typeof createResourceEvents
    >[0]["tokenRates"],
    openEvents: openServerEvents,
  });
  transport.watchResourceChanges({ kind: "state" }, () => {});
  let stop: (() => void) | undefined;
  const select = (agentId: string) => {
    stop?.();
    stop = transport.watchResourceChanges(
      { kind: "transcript", agentId },
      () => {
        document.querySelector("output")!.textContent = agentId;
      },
    );
  };
  document.querySelector<HTMLButtonElement>("#one")!.onclick = () =>
    select("one");
  document.querySelector<HTMLButtonElement>("#two")!.onclick = () =>
    select("two");
  window.transport = transport;
  select("one");
};
