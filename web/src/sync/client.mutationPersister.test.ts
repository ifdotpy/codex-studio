import { afterEach, describe, expect, it, vi } from "vitest";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
import { getEntitySequenceCheckpoint } from "./entitySequence";

const mocks = vi.hoisted(() => ({
  createRxDatabase: vi.fn(),
  syncGet: vi.fn(),
  registerPersister: vi.fn(),
  persist: undefined as
    | ((
        workspaceId: string,
        documents: Array<{ id: string; seq: number; payload: string }>,
        after: number | null | undefined,
        current: () => boolean,
      ) => Promise<void>)
    | undefined,
}));

vi.mock("rxdb", () => ({
  addRxPlugin: vi.fn(),
  createRxDatabase: mocks.createRxDatabase,
}));
vi.mock("rxdb/plugins/storage-dexie", () => ({
  getRxStorageDexie: vi.fn(() => ({})),
}));
vi.mock("rxdb/plugins/leader-election", () => ({
  RxDBLeaderElectionPlugin: {},
}));
vi.mock("rxdb/plugins/replication", () => ({
  replicateRxCollection: vi.fn(),
}));
vi.mock("../api", () => ({
  ApiError: class ApiError extends Error {
    constructor(
      message: string,
      public status: number,
    ) {
      super(message);
    }
  },
  isApiSchemaMismatch: vi.fn(() => false),
  onMatchingApiSchemaResponse: vi.fn(() => () => {}),
  registerSyncEntityPersister: (persist: typeof mocks.persist) => {
    mocks.persist = persist;
    mocks.registerPersister(persist);
  },
  save: vi.fn(),
  saved: (_key: string, fallback: unknown) => fallback,
  setWorkspace: vi.fn(),
  syncGet: mocks.syncGet,
  syncPost: vi.fn(),
}));
vi.mock("./resourceEvents", () => ({
  acknowledgeEntitySequences: vi.fn(),
  watchResourceChanges: vi.fn(() => () => {}),
  watchResourceConnection: vi.fn(() => () => {}),
}));
vi.mock("./resume", () => ({ onResume: vi.fn(() => () => {}) }));

describe("registered mutation entity persister", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    mocks.createRxDatabase.mockReset();
    mocks.syncGet.mockReset();
    mocks.registerPersister.mockReset();
    mocks.persist = undefined;
  });

  it("reconciles another tab's durable checkpoint before advancing this response", async () => {
    const workspaceId = "f".repeat(32);
    const rows = new Map<string, Record<string, unknown>>([
      [
        "state:entities:checkpoint",
        {
          id: "state:entities:checkpoint",
          payload: "{}",
          seq: 10,
          _deleted: false,
        },
      ],
    ]);
    const storageInstance = {
      findDocumentsById: vi.fn(async (ids: string[]) =>
        ids.flatMap((id) => (rows.has(id) ? [rows.get(id)!] : [])),
      ),
      bulkWrite: vi.fn(
        async (writes: Array<{ document: Record<string, unknown> }>) => {
          for (const { document } of writes)
            rows.set(String(document.id), document);
          return { error: [] };
        },
      ),
    };
    const projections = {
      storageInstance,
      $: { subscribe: vi.fn(() => ({ unsubscribe: vi.fn() })) },
      find: vi.fn(() => ({
        exec: vi.fn(async () => []),
        $: { subscribe: vi.fn(() => ({ unsubscribe: vi.fn() })) },
      })),
      findOne: vi.fn(() => ({
        $: { subscribe: vi.fn(() => ({ unsubscribe: vi.fn() })) },
      })),
    };
    mocks.createRxDatabase
      .mockResolvedValueOnce({ addCollections: vi.fn(async () => ({})) })
      .mockResolvedValueOnce({
        addCollections: vi.fn(async () => ({ projections })),
      });
    mocks.syncGet.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      __studioSyncEntityPersisterProbe: vi.fn(),
    });
    vi.stubGlobal("document", { hidden: false });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });

    await import("./client");
    expect(mocks.persist).toBeTypeOf("function");
    getEntitySequenceCheckpoint(
      workspaceId,
      "state:entities:v1",
      API_SCHEMA_HASH,
    ).assign(9);
    await mocks.persist?.(
      workspaceId,
      [{ id: "entity:agent:chat", payload: "{}", seq: 12 }],
      10,
      () => true,
    );

    expect(rows.get("entity:agent:chat")?.seq).toBe(12);
    expect(rows.get("state:entities:checkpoint")?.seq).toBe(12);
    expect(
      (
        window as unknown as Window & {
          __studioSyncEntityPersisterProbe: (value: {
            advanced: boolean;
          }) => void;
        }
      ).__studioSyncEntityPersisterProbe,
    ).toHaveBeenCalledWith(expect.objectContaining({ advanced: true }));
  });
});
