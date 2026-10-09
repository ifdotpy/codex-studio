import { afterEach, describe, expect, it, vi } from "vitest";
import { API_SCHEMA_HASH, API_SCHEMA_HASH_HEADER } from "./generated/apiSchema";

describe("mutation entity coverage without an open projection", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("does not create coverage before the first projection subscription", async () => {
    vi.resetModules();
    vi.stubGlobal("window", new EventTarget());
    vi.stubGlobal("document", new EventTarget());
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json(
          {
            _syncEntitiesAfter: 10,
            _syncEntities: [
              {
                id: "entity:workspace:current",
                seq: 12,
                payload: "{}",
                _deleted: false,
              },
            ],
          },
          { headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH } },
        ),
      ),
    );

    const api = await import("./api");
    const entitySequence = await import("./sync/entitySequence");
    api.setWorkspace("workspace-a");
    const persister = vi.fn(async () => {});
    const unregister = api.registerSyncEntityPersister(persister);
    try {
      await api.post("/api/sync/drafts", { rows: [] });
      expect(persister).toHaveBeenCalledOnce();
      expect(
        entitySequence.getEntitySequenceCheckpointForWorkspace(
          "workspace-a",
          "state:entities:v1",
          API_SCHEMA_HASH,
        ),
      ).toBeUndefined();
    } finally {
      unregister();
    }
  });
});
