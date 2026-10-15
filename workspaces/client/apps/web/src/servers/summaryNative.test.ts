import { afterEach, expect, it, vi } from "vitest";
vi.mock("../native/client", () => ({ hasNativeClient: () => true }));
vi.mock("../native/http", () => ({ nativeFetch: vi.fn() }));
import { nativeFetch } from "../native/http";
import { fetchServerSummary } from "./summary";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
afterEach(() => vi.unstubAllGlobals());
it("loads the local shell summary through the verified native backend", async () => {
  vi.stubGlobal("window", {});
  const fetch = vi.fn();
  vi.stubGlobal("fetch", fetch);
  vi.mocked(nativeFetch).mockResolvedValue(
    new Response(
      JSON.stringify({
        chats: [],
        projects: [],
        alerts: [],
        accounts: [],
        ready: true,
        busy: false,
        system: "fixture",
        agentsRunning: 0,
      }),
      { headers: { "x-studio-api-schema": API_SCHEMA_HASH } },
    ),
  );
  const server = { id: "local", label: "Local", origin: "http://localhost" };
  expect(
    (await fetchServerSummary(server, new AbortController().signal)).ready,
  ).toBe(true);
  expect(nativeFetch).toHaveBeenCalledOnce();
  expect(vi.mocked(nativeFetch).mock.calls[0][0].url).toBe(
    "http://localhost/api/ui-summary",
  );
  expect(fetch).not.toHaveBeenCalled();
});
