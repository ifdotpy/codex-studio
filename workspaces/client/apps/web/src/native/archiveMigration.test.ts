import { createHash } from "node:crypto";
import { afterEach, describe, expect, it, vi } from "vitest";
import { migrateNativeArchive } from "./archiveMigration";
import { combineBrowserProfiles } from "./combineBrowserProfiles.mjs";
import type { NativeArchiveOptions } from "./archiveMigration";
import type { NativeServerContext } from "./client";

type Json = null | string | number | boolean | Json[] | { [key: string]: Json };
const workspace = "a".repeat(32);
const hash = (value: string | Buffer) =>
  createHash("sha256").update(value).digest("hex");
function tag(value: Json): unknown[] {
  if (Array.isArray(value)) return ["array", value.map(tag)];
  if (value !== null && typeof value === "object")
    return [
      "object",
      Object.entries(value).map(([key, value]) => [key, tag(value)]),
    ];
  return ["value", value];
}
const object = (fields: Record<string, unknown[]>) => [
  "object",
  Object.entries(fields),
];
const record = (key: Json, value: unknown[]) => [tag(key), value];
const savedStore = (name: string, records: unknown[]) => ({
  name,
  keyPath: null,
  autoIncrement: false,
  indexes: [],
  records,
});

function fixture() {
  const draftPayload = JSON.stringify({
    id: "phone:chat",
    device: "phone",
    session: "chat",
    text: "saved 🦀",
    updated: 7,
  });
  const tombstonePayload = JSON.stringify({
    id: "old:chat",
    device: "old",
    session: "chat",
    text: "",
    updated: 8,
  });
  const pending = {
    id: "preference-old",
    method: "preferences.raw_set",
    params: { key: "theme", raw: "dark" },
  };
  const outbox = {
    body: { id: "message-old", room: "chat", text: "pending" },
    status: "uncertain",
    attempted: true,
    created: 1,
    displayPending: true,
    attachments: [],
  };
  const upload = {
    id: "upload-old",
    workspace,
    stateDir: "test-state",
    agent: "chat",
    name: "bytes.bin",
    mime: "application/octet-stream",
    size: 3,
    created: 1,
  };
  const recording = {
    id: "recording-old",
    chatId: "chat",
    created: 2,
    sampleRate: 16000,
    samples: 3,
    state: "ready",
    deletedAt: 4,
    transcriptionAttempt: "transcription-old",
  };
  const snapshot = {
    version: 1,
    origin: "http://fixture.invalid",
    preferences: {
      theme: "dark",
      "studio-native-preferences-journal-v1": JSON.stringify([pending]),
      [`studio-native-draft-journal-v1:${workspace}:draft-old`]: JSON.stringify(
        {
          version: 1,
          workspace,
          sequence: 1,
          edit: { id: "draft-old", changes: { chat: "unsent" } },
        },
      ),
      "studio-upload-cancelled:cancelled-old": "true",
    },
    databases: [
      {
        name: `rxdb-dexie-studio${workspace}--0--drafts`,
        version: 1,
        stores: [
          savedStore("docs", [
            record(
              "phone:chat",
              tag({
                id: "phone:chat",
                payload: draftPayload,
                seq: 5,
                _deleted: false,
              }),
            ),
            record(
              "old:chat",
              tag({
                id: "old:chat",
                payload: tombstonePayload,
                seq: 6,
                _deleted: true,
              }),
            ),
          ]),
        ],
      },
      {
        name: `rxdb-dexie-studio${workspace}--0--outbox`,
        version: 1,
        stores: [
          savedStore("docs", [
            record(
              "message-old",
              tag({
                id: "message-old",
                payload: JSON.stringify(outbox),
                seq: 1,
                _deleted: false,
              }),
            ),
          ]),
        ],
      },
      {
        name: "studio-monitor-input-journal-v1",
        version: 1,
        stores: [
          savedStore("inputs", [
            record(
              ["local", "monitor-old"],
              tag({
                requestId: "input-old",
                payloadDigest: "b".repeat(64),
                uncertain: true,
              }),
            ),
          ]),
        ],
      },
      {
        name: "codex-studio-uploads",
        version: 2,
        stores: [
          savedStore("uploads", [record("upload-old", tag(upload))]),
          savedStore("contents", [
            record(
              "upload-old",
              object({ id: tag("upload-old"), bytes: ["bytes", "AP8q"] }),
            ),
          ]),
        ],
      },
      {
        name: "codex-studio-dictation",
        version: 1,
        stores: [
          savedStore("recordings", [record("recording-old", tag(recording))]),
          savedStore("chunks", [
            record(
              ["recording-old", 0],
              object({
                id: tag("recording-old"),
                index: tag(0),
                pcm: ["view", "Int16Array", ["bytes", "AIAAAA=="]],
              }),
            ),
            record(
              ["recording-old", 1],
              object({
                id: tag("recording-old"),
                index: tag(1),
                pcm: ["view", "Int16Array", ["bytes", "/38="]],
              }),
            ),
          ]),
        ],
      },
      {
        name: `rxdb-dexie-studio${workspace}--0--projections`,
        version: 1,
        stores: [
          savedStore("docs", [
            record(
              "transcript:A",
              tag({ id: "transcript:A", payload: "cached", seq: 1 }),
            ),
          ]),
        ],
      },
    ],
  };
  const serializedSnapshot = JSON.stringify(snapshot);
  const archive = {
    id: "sealed-fixture",
    profiles: [{ owner: "root", snapshot }],
  };
  const sourceIdentity = "c".repeat(64);
  const options: NativeArchiveOptions = {
    archive,
    sourceIdentity,
    archiveProof: {
      id: archive.id,
      owner: "root",
      sha256: hash(serializedSnapshot),
      serializedSnapshot,
    },
    verify: vi.fn(async () => {}),
    readChunk: vi.fn(async () => {
      throw new Error("No external chunk in this fixture.");
    }),
  };
  const hashes = new Map<
    string,
    { size: number; chunks: Buffer[]; digest?: string }
  >();
  const imports = new Map<
    string,
    { manifest: any; chunks: Map<number, Buffer[]>; receipt?: any }
  >();
  const metadata = new Map<string, any>();
  const request = vi.fn(async (method: string, p: any): Promise<any> => {
    if (method === "files.hashBegin") {
      let row = hashes.get(p.id);
      if (!row) {
        row = { size: p.bytes, chunks: [] };
        hashes.set(p.id, row);
      }
      expect(row.size).toBe(p.bytes);
      return { chunkBytes: 2, bytes: row.size, sha256: row.digest ?? null };
    }
    if (method === "files.hashChunk") {
      const row = hashes.get(p.id)!;
      const bytes = Buffer.from(p.base64, "base64");
      expect(bytes.length).toBeLessThanOrEqual(2);
      const old = Buffer.concat(row.chunks);
      if (p.offset < old.length)
        expect(old.subarray(p.offset, p.offset + bytes.length)).toEqual(bytes);
      else {
        expect(p.offset).toBe(old.length);
        row.chunks.push(bytes);
      }
      return { nextOffset: p.offset + bytes.length };
    }
    if (method === "files.hashFinish") {
      const row = hashes.get(p.id)!;
      const bytes = Buffer.concat(row.chunks);
      expect(bytes.length).toBe(row.size);
      row.digest ??= hash(bytes);
      return { bytes: row.size, sha256: row.digest };
    }
    if (method === "files.importBegin") {
      let row = imports.get(p.manifest.id);
      if (row) expect(row.manifest).toEqual(p.manifest);
      else {
        row = { manifest: p.manifest, chunks: new Map() };
        imports.set(p.manifest.id, row);
      }
      return {
        manifestId: p.manifest.id,
        manifestHash: hash(JSON.stringify(p.manifest)),
        chunkBytes: 2,
        status: row.receipt ? "complete" : "pending",
      };
    }
    if (method === "files.importChunk") {
      const row = imports.get(p.manifestId)!;
      const bytes = Buffer.from(p.base64, "base64");
      expect(bytes.length).toBeLessThanOrEqual(2);
      const chunks = row.chunks.get(p.entry) ?? [];
      const old = Buffer.concat(chunks);
      if (p.offset < old.length)
        expect(old.subarray(p.offset, p.offset + bytes.length)).toEqual(bytes);
      else {
        expect(p.offset).toBe(old.length);
        chunks.push(bytes);
        row.chunks.set(p.entry, chunks);
      }
      return { nextOffset: p.offset + bytes.length };
    }
    if (method === "files.importCommit") {
      const row = imports.get(p.manifestId)!;
      const manifest = row.manifest;
      for (const [index, entry] of manifest.entries.entries()) {
        const bytes = Buffer.concat(row.chunks.get(index) ?? []);
        expect(bytes.length).toBe(entry.bytes);
        expect(hash(bytes)).toBe(entry.sha256);
      }
      return (row.receipt ??= {
        manifestId: manifest.id,
        manifestHash: hash(JSON.stringify(manifest)),
        source: manifest.source,
        workspace: manifest.workspace,
        entries: manifest.entries.map((entry: any) => ({
          id: entry.id,
          kind: entry.kind,
          bytes: entry.bytes,
          sha256: entry.sha256,
          documentHash: hash(JSON.stringify(entry.document ?? null)),
          cancelled: entry.cancelled ?? null,
        })),
      });
    }
    if (method === "files.importReadback") {
      const row = imports.get(p.manifestId)!;
      if (!row.receipt) throw new Error("Not committed");
      return row.receipt;
    }
    if (method === "client_storage.import") {
      const source = JSON.parse(p.serializedSource);
      const receipt = {
        manifestId: p.manifestId,
        sourceSha256: hash(p.serializedSource),
        imported: true,
        sourceIdentity: source.sourceIdentity,
        expectedByteImports: source.expectedByteImports,
        archiveManifest: source.archiveManifest,
      };
      const old = metadata.get(p.manifestId);
      if (old) expect(old).toEqual(receipt);
      else metadata.set(p.manifestId, receipt);
      return receipt;
    }
    if (method === "client_storage.status")
      return metadata.get(p.manifestId) ?? null;
    throw new Error(`Unexpected native request ${method}`);
  });
  const context = {
    serverId: "local",
    workspaceId: workspace,
    request,
  } as unknown as NativeServerContext;
  return {
    context,
    request,
    options,
    snapshot,
    archive,
    imports,
    metadata,
    draftPayload,
    tombstonePayload,
    upload,
    recording,
  };
}

afterEach(() => vi.unstubAllGlobals());
describe("sealed native archive migration", () => {
  it("refuses foreign source preferences and wrong byte chunk checksums before import", async () => {
    const owner = fixture();
    const context = { ...owner.context, serverId: "remote" };
    owner.archive.profiles[0].owner = "server:remote";
    owner.snapshot.databases = [];
    owner.options.archiveProof = {
      ...owner.options.archiveProof,
      owner: "server:remote",
      serializedSnapshot: JSON.stringify(owner.snapshot),
      sha256: hash(JSON.stringify(owner.snapshot)),
    };
    await expect(migrateNativeArchive(context, owner.options)).rejects.toThrow(
      /foreign|scope|owner/i,
    );
    expect(
      owner.request.mock.calls.some(
        ([method]) =>
          method === "files.importBegin" || method === "client_storage.import",
      ),
    ).toBe(false);
    const corrupt = fixture();
    corrupt.snapshot.version = 2;
    for (const db of corrupt.snapshot.databases)
      for (const saved of db.stores as any[]) {
        const source = JSON.stringify(saved.records);
        saved.chunks = [
          {
            sha256: hash(source),
            bytes: Buffer.byteLength(source),
            records: saved.records.length,
          },
        ];
        delete saved.records;
      }
    corrupt.options.archiveProof = {
      ...corrupt.options.archiveProof,
      serializedSnapshot: JSON.stringify(corrupt.snapshot),
      sha256: hash(JSON.stringify(corrupt.snapshot)),
    };
    corrupt.options.readChunk = vi.fn(async () => "[]");
    await expect(
      migrateNativeArchive(corrupt.context, corrupt.options),
    ).rejects.toThrow(/chunk|hash|checksum/i);
    expect(
      corrupt.request.mock.calls.some(
        ([method]) =>
          method === "files.importBegin" || method === "client_storage.import",
      ),
    ).toBe(false);
  });
  it("retains known special cache tags and recovers legacy draft values without browser IO", async () => {
    const owner = fixture();
    owner.snapshot.databases[5].stores[0].records.push(
      record("special", [
        "object",
        [
          ["date", ["date", "2026-01-01T00:00:00.000Z"]],
          ["undefined", ["undefined"]],
          ["nonfinite", ["number", "NaN"]],
        ],
      ]),
    );
    Object.assign(owner.snapshot.preferences, {
      [`codex-chat-draft:${workspace}:chat`]: JSON.stringify({
        version: 1,
        workspace,
        session: "chat",
        text: "new local branch",
        deleted: false,
        source: "local",
        updated: 9,
      }),
    });
    owner.options.archiveProof = {
      ...owner.options.archiveProof,
      serializedSnapshot: JSON.stringify(owner.snapshot),
      sha256: hash(JSON.stringify(owner.snapshot)),
    };
    await migrateNativeArchive(owner.context, owner.options);
    const source = JSON.parse(
      owner.request.mock.calls.find(
        ([method]) => method === "client_storage.import",
      )![1].serializedSource,
    );
    expect(source.drafts[0].versions).toEqual([
      expect.objectContaining({
        id: "browser-import:chat",
        text: "new local branch",
        updated: 9,
      }),
    ]);
    expect(source.archiveManifest.excludedCaches[0].records).toBe(2);
  });

  it("names exact derived cache exclusions and never discards nonempty message attachment stores", async () => {
    const owner = fixture();
    owner.snapshot.databases[0].stores.push(
      savedStore("changes", []),
      savedStore("attachments", []),
    );
    owner.snapshot.databases.push({
      name: `rxdb-dexie-studio-entity-projection-${workspace}-${"d".repeat(64)}--1--projections`,
      version: 1,
      stores: [
        savedStore("docs", [
          record("entity", tag({ id: "entity", payload: "cache" })),
        ]),
      ],
    });
    owner.options.archiveProof = {
      ...owner.options.archiveProof,
      serializedSnapshot: JSON.stringify(owner.snapshot),
      sha256: hash(JSON.stringify(owner.snapshot)),
    };
    const result = await migrateNativeArchive(owner.context, owner.options);
    expect(
      result.archiveManifest.excludedCaches.map((row) => [
        row.store,
        row.records,
      ]),
    ).toEqual([
      ["changes", 0],
      ["attachments", 0],
      ["docs", 1],
      ["docs", 1],
    ]);
    const unknown = fixture();
    unknown.snapshot.databases[1].stores.push(
      savedStore("attachments", [record("saved", ["bytes", "AP8q"])]),
    );
    unknown.options.archiveProof = {
      ...unknown.options.archiveProof,
      serializedSnapshot: JSON.stringify(unknown.snapshot),
      sha256: hash(JSON.stringify(unknown.snapshot)),
    };
    await expect(
      migrateNativeArchive(unknown.context, unknown.options),
    ).rejects.toThrow(/unsupported|attachment/i);
    expect(
      unknown.request.mock.calls.some(
        ([method]) =>
          method === "files.importBegin" || method === "client_storage.import",
      ),
    ).toBe(false);
  });

  it("imports original metadata IDs, exact PCM chunks and settlements through native owners", async () => {
    const owner = fixture();
    const original = JSON.stringify(owner.archive);
    for (const name of ["indexedDB", "localStorage", "crypto", "File", "Blob"])
      vi.stubGlobal(
        name,
        new Proxy(function () {}, {
          get() {
            throw new Error(`Browser ${name} was accessed`);
          },
          construct() {
            throw new Error(`Browser ${name} was constructed`);
          },
        }),
      );
    const result = await migrateNativeArchive(owner.context, owner.options);
    expect(result.mode).toBe("direct");
    expect(JSON.stringify(owner.archive)).toBe(original);
    const calls = owner.request.mock.calls.filter(
      ([method]) => method === "client_storage.import",
    );
    expect(calls).toHaveLength(1);
    const source = JSON.parse(calls[0][1].serializedSource);
    expect(source.pendingPreferences[0].id).toBe("preference-old");
    expect(source.pendingDrafts[0]).toEqual({
      workspace,
      id: "draft-old",
      changes: { chat: "unsent" },
    });
    expect(
      source.draftDocuments[0].documents.map((row: any) => [
        row.id,
        row.payload,
        row._deleted,
      ]),
    ).toEqual([
      ["phone:chat", owner.draftPayload, false],
      ["old:chat", owner.tombstonePayload, true],
    ]);
    expect(source.monitorInputs).toEqual([
      {
        monitorId: "monitor-old",
        requestId: "input-old",
        payloadDigest: "b".repeat(64),
        uncertain: true,
      },
    ]);
    expect(source.outbox[0]).toMatchObject({
      id: "message-old",
      workspace,
      status: "uncertain",
      attempted: true,
    });
    expect(result.bytes).toHaveLength(3);
    expect(source.expectedByteImports).toEqual(
      result.bytes.map((row) => row.manifestId),
    );
    const entries = [...owner.imports.values()].flatMap(
      (row) => row.manifest.entries,
    );
    expect(entries.find((row) => row.id === "upload-old").document).toEqual(
      owner.upload,
    );
    expect(entries.find((row) => row.id === "recording-old").document).toEqual(
      owner.recording,
    );
    const pcm = [...owner.imports.values()].find(
      (row) => row.manifest.entries[0].id === "recording-old",
    )!;
    expect(Buffer.concat(pcm.chunks.get(0)!)).toEqual(
      Buffer.from([0, 128, 0, 0, 255, 127]),
    );
    expect(entries.find((row) => row.id === "cancelled-old")).toMatchObject({
      kind: "uploadTombstone",
      cancelled: true,
      bytes: 0,
    });
    expect(result.archiveManifest.excludedCaches).toEqual([
      expect.objectContaining({
        database: `rxdb-dexie-studio${workspace}--0--projections`,
        store: "docs",
        records: 1,
        sha256: expect.stringMatching(/^[a-f0-9]{64}$/),
      }),
    ]);
  });

  it("verifies external tagged chunks against the sealed owner before any import", async () => {
    const owner = fixture();
    const store = owner.snapshot.databases[3].stores[1] as any;
    const text = JSON.stringify(store.records);
    const reference = {
      sha256: hash(text),
      bytes: Buffer.byteLength(text),
      records: store.records.length,
    };
    delete store.records;
    store.chunks = [reference];
    owner.snapshot.version = 2;
    for (const db of owner.snapshot.databases)
      for (const saved of db.stores as any[])
        if (saved.records) {
          saved.chunks = [
            {
              sha256: hash(JSON.stringify(saved.records)),
              bytes: Buffer.byteLength(JSON.stringify(saved.records)),
              records: saved.records.length,
            },
          ];
        }
    const chunks = new Map(
      owner.snapshot.databases
        .flatMap((db) =>
          db.stores.map(
            (saved) =>
              [JSON.stringify((saved as any).records), saved as any] as const,
          ),
        )
        .filter(([records]) => records !== undefined)
        .map(([records, saved]) => [saved.chunks[0].sha256, records]),
    );
    chunks.set(reference.sha256, text);
    for (const db of owner.snapshot.databases)
      for (const saved of db.stores as any[]) delete saved.records;
    owner.options.archiveProof = {
      ...owner.options.archiveProof,
      serializedSnapshot: JSON.stringify(owner.snapshot),
      sha256: hash(JSON.stringify(owner.snapshot)),
    };
    owner.options.readChunk = vi.fn(async (source, digest) => {
      expect(source).toBe("root");
      return chunks.get(digest)!;
    });
    await migrateNativeArchive(owner.context, owner.options);
    expect(owner.options.readChunk).toHaveBeenCalled();
    const altered = fixture();
    altered.options.archiveProof = {
      ...altered.options.archiveProof,
      sha256: "0".repeat(64),
    };
    await expect(
      migrateNativeArchive(altered.context, altered.options),
    ).rejects.toThrow(/seal|hash|checksum/i);
    expect(
      altered.request.mock.calls.some(
        ([method]) =>
          method === "client_storage.import" || method === "files.importBegin",
      ),
    ).toBe(false);
  });

  it("refuses unknown stores and tags instead of silently excluding saved intentions", async () => {
    const owner = fixture();
    owner.snapshot.databases.push({
      name: "studio-automatic-ui-access-v1",
      version: 3,
      stores: [
        savedStore("attempts", [
          record(
            "server",
            tag({ inviteRequestId: "invite-old", pairRequestId: "pair-old" }),
          ),
        ]),
      ],
    });
    owner.options.archiveProof = {
      ...owner.options.archiveProof,
      serializedSnapshot: JSON.stringify(owner.snapshot),
      sha256: hash(JSON.stringify(owner.snapshot)),
    };
    await expect(
      migrateNativeArchive(owner.context, owner.options),
    ).rejects.toThrow(/unsupported|unrepresented|unknown/i);
    expect(
      owner.request.mock.calls.some(
        ([method]) =>
          method === "client_storage.import" || method === "files.importBegin",
      ),
    ).toBe(false);
    const invalid = fixture();
    (invalid.snapshot.databases[0].stores[0].records[0] as any)[1] = [
      "unknown-tag",
      "saved",
    ];
    invalid.options.archiveProof = {
      ...invalid.options.archiveProof,
      serializedSnapshot: JSON.stringify(invalid.snapshot),
      sha256: hash(JSON.stringify(invalid.snapshot)),
    };
    await expect(
      migrateNativeArchive(invalid.context, invalid.options),
    ).rejects.toThrow(/unsupported|tag|unknown/i);
    expect(
      invalid.request.mock.calls.some(
        ([method]) =>
          method === "client_storage.import" || method === "files.importBegin",
      ),
    ).toBe(false);
  });

  it("reuses completed native import identities after response loss without another byte write", async () => {
    const owner = fixture();
    const first = await migrateNativeArchive(owner.context, owner.options);
    const writes = owner.request.mock.calls.filter(
      ([method]) =>
        method === "files.importChunk" || method === "files.hashChunk",
    ).length;
    const second = await migrateNativeArchive(owner.context, owner.options);
    expect(second).toEqual(first);
    expect(
      owner.request.mock.calls.filter(
        ([method]) =>
          method === "files.importChunk" || method === "files.hashChunk",
      ),
    ).toHaveLength(writes);
    expect(
      owner.request.mock.calls.filter(
        ([method]) => method === "files.importReadback",
      ),
    ).toHaveLength(first.bytes.length * 2);
  });

  it("does not commit metadata or report completion when byte readback differs", async () => {
    const owner = fixture();
    const original = owner.request.getMockImplementation()!;
    owner.request.mockImplementation(async (method, p) => {
      const result = await original(method, p);
      return method === "files.importReadback"
        ? { ...result, manifestHash: "0".repeat(64) }
        : result;
    });
    await expect(
      migrateNativeArchive(owner.context, owner.options),
    ).rejects.toThrow(/readback|receipt|verify/i);
    expect(
      owner.request.mock.calls.some(
        ([method]) => method === "client_storage.import",
      ),
    ).toBe(false);
  });

  it("adopts current source only for the exact complete legacy marker without replaying old rows", async () => {
    const owner = fixture();
    const combined = combineBrowserProfiles(owner.archive.profiles);
    owner.options.legacyImportMarker = `${owner.archive.id}:${hash(JSON.stringify(combined))}:complete`;
    const result = await migrateNativeArchive(owner.context, owner.options);
    expect(result.mode).toBe("adopt-current");
    expect(result.metadata).toEqual([]);
    expect(result.bytes).toEqual([]);
    expect(result.archiveManifest.legacyAdoption?.marker).toBe(
      owner.options.legacyImportMarker,
    );
    expect(
      owner.request.mock.calls.some(
        ([method]) =>
          method === "client_storage.import" || method === "files.importBegin",
      ),
    ).toBe(false);
    const wrong = fixture();
    wrong.options.legacyImportMarker = `${wrong.archive.id}:${"0".repeat(64)}:complete`;
    await expect(
      migrateNativeArchive(wrong.context, wrong.options),
    ).rejects.toThrow(/marker|identity|archive/i);
    expect(
      wrong.request.mock.calls.some(
        ([method]) =>
          method === "client_storage.import" || method === "files.importBegin",
      ),
    ).toBe(false);
  });
});
