import { afterEach, describe, expect, it, vi } from "vitest";
import { API_SCHEMA_HASH, API_SCHEMA_HASH_HEADER } from "./generated/apiSchema";

describe("mutation response entity delivery", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("returns the operation result without a response entity side channel", async () => {
    vi.stubGlobal("window", new EventTarget());
    vi.stubGlobal("document", new EventTarget());
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json(
          { requestId: "mutation-1", updated: true },
          { headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH } },
        ),
      ),
    );

    const api = await import("./api");
    api.setToken("test-token");
    api.setWorkspace("workspace-a");
    await expect(api.post("/api/sync/drafts", { rows: [] })).resolves.toEqual({
      requestId: "mutation-1",
      updated: true,
    });
  });
});
