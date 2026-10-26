import type { NativeArchiveSeal, NativeServerContext } from "./client";
import { combineBrowserProfiles } from "./combineBrowserProfiles.mjs";
import { validMonitorInput } from "../servers/monitorInputStore";
import { readPendingLegacyPreferences } from "./preferenceJournal";
import { DraftJournal } from "./draftJournal";
import { entityProjectionDatabaseName } from "../sync/entityCacheStorage";
import { readLegacyDraftValues } from "./legacyDraftValues";
import type { DraftVersion } from "../sync/drafts";

export type NativeArchiveManifest = {
  version: 1;
  archive: {
    id: string;
    owner: string;
    sha256: string;
    sourceIdentity: string;
  };
  excludedCaches: {
    database: string;
    store: string;
    records: number;
    sha256: string;
  }[];
  legacyAdoption?: { marker: string; combinedSnapshotSha256: string };
};
export type NativeArchiveOptions = {
  archive: unknown;
  sourceIdentity: string;
  archiveProof: NativeArchiveSeal;
  verify(): Promise<void>;
  readChunk(owner: string, sha256: string): Promise<unknown>;
  legacyImportMarker?: string | null;
};
export type NativeArchiveResult = {
  mode: "direct" | "adopt-current";
  metadata: { manifestId: string; sourceSha256: string }[];
  bytes: { manifestId: string; receipt: unknown }[];
  archiveManifest: NativeArchiveManifest;
};

type ObjectValue = Record<string, unknown>;
type ByteSource = {
  bytes: number;
  read(offset: number, length: number): Uint8Array | Promise<Uint8Array>;
};
type SourceAt = (path: number[], value: string) => ByteSource;
type Binary = { binary: ByteSource; view?: string };
type Artifact = {
  kind: "upload" | "recording" | "uploadTombstone";
  id: string;
  source: ByteSource;
  document?: ObjectValue;
  cancelled?: boolean;
};
const alphabet =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const digestPattern = /^[a-f0-9]{64}$/;
function fail(message: string): never {
  throw new Error(message + " The source archive was preserved.");
}
function object(value: unknown): ObjectValue {
  if (!value || typeof value !== "object" || Array.isArray(value))
    fail("Invalid archive object.");
  return value as ObjectValue;
}
function array(value: unknown): unknown[] {
  if (!Array.isArray(value)) fail("Invalid archive array.");
  return value;
}
function text(value: unknown): string {
  if (typeof value !== "string") fail("Invalid archive string.");
  return value;
}
function integer(value: unknown): number {
  if (!Number.isSafeInteger(value) || (value as number) < 0)
    fail("Invalid archive count.");
  return value as number;
}
function equal(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (Array.isArray(a) && Array.isArray(b))
    return a.length === b.length && a.every((v, i) => equal(v, b[i]));
  if (a && b && typeof a === "object" && typeof b === "object") {
    const left = Object.keys(a),
      right = Object.keys(b);
    return (
      left.length === right.length &&
      left.every(
        (key) =>
          Object.hasOwn(b, key) &&
          equal((a as ObjectValue)[key], (b as ObjectValue)[key]),
      )
    );
  }
  return false;
}
function base64(bytes: Uint8Array): string {
  let result = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const a = bytes[i],
      b = bytes[i + 1],
      c = bytes[i + 2];
    result +=
      alphabet[a >> 2] +
      alphabet[((a & 3) << 4) | ((b ?? 0) >> 4)] +
      (b === undefined ? "=" : alphabet[((b & 15) << 2) | ((c ?? 0) >> 6)]) +
      (c === undefined ? "=" : alphabet[c & 63]);
  }
  return result;
}
function encodedSource(value: string): ByteSource {
  if (
    !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(
      value,
    )
  )
    fail("Invalid archive base64.");
  const bytes =
    (value.length / 4) * 3 -
    (value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0);
  const result: ByteSource = {
    bytes,
    read(offset, length) {
      const out = new Uint8Array(length);
      for (let i = 0; i < length; i++) {
        const index = offset + i,
          group = Math.floor(index / 3) * 4,
          part = index % 3;
        const a = alphabet.indexOf(value[group]),
          b = alphabet.indexOf(value[group + 1]),
          c = alphabet.indexOf(value[group + 2]),
          d = alphabet.indexOf(value[group + 3]);
        out[i] =
          part === 0
            ? (a << 2) | (b >> 4)
            : part === 1
              ? (b << 4) | (c >> 2)
              : (c << 6) | d;
      }
      return out;
    },
  };
  if (
    (value.endsWith("==") &&
      (alphabet.indexOf(value[value.length - 3]) & 15) !== 0) ||
    (value.endsWith("=") &&
      !value.endsWith("==") &&
      (alphabet.indexOf(value[value.length - 2]) & 3) !== 0)
  )
    fail("Invalid archive byte encoding.");
  return result;
}
function joined(parts: ByteSource[]): ByteSource {
  return {
    bytes: parts.reduce((n, p) => n + p.bytes, 0),
    async read(offset, length) {
      const out = new Uint8Array(length);
      let written = 0;
      for (const part of parts) {
        if (offset >= part.bytes) {
          offset -= part.bytes;
          continue;
        }
        const n = Math.min(length - written, part.bytes - offset);
        out.set(await part.read(offset, n), written);
        written += n;
        offset = 0;
        if (written === length) break;
      }
      return out;
    },
  };
}
function utf8(value: string): ByteSource {
  const bytes = new TextEncoder().encode(value);
  return {
    bytes: bytes.length,
    read: (offset, length) => bytes.subarray(offset, offset + length),
  };
}
function decode(
  value: unknown,
  sourceAt?: SourceAt,
  path: number[] = [],
): unknown {
  const tag = array(value),
    kind = text(tag[0]);
  if (
    kind === "value" &&
    tag.length === 2 &&
    (tag[1] === null ||
      ["string", "boolean"].includes(typeof tag[1]) ||
      (typeof tag[1] === "number" && Number.isFinite(tag[1])))
  )
    return tag[1];
  if (kind === "array" && tag.length === 2)
    return array(tag[1]).map((value, index) =>
      decode(value, sourceAt, [...path, 1, index]),
    );
  if (kind === "object" && tag.length === 2) {
    const result = Object.create(null) as ObjectValue;
    for (const [index, field] of array(tag[1]).entries()) {
      const pair = array(field),
        key = text(pair[0]);
      if (pair.length !== 2 || Object.hasOwn(result, key))
        fail("Duplicate archive field.");
      result[key] = decode(pair[1], sourceAt, [...path, 1, index, 1]);
    }
    return result;
  }
  if (kind === "bytes" && tag.length === 2)
    return {
      binary: sourceAt
        ? sourceAt([...path, 1], text(tag[1]))
        : encodedSource(text(tag[1])),
    };
  if (kind === "view" && tag.length === 3) {
    if (
      ![
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float32Array",
        "Float64Array",
        "BigInt64Array",
        "BigUint64Array",
        "DataView",
      ].includes(text(tag[1]))
    )
      fail("Unsupported archive view.");
    return { ...object(decode(tag[2], sourceAt, [...path, 2])), view: tag[1] };
  }
  if (
    (kind === "blob" && tag.length === 3) ||
    (kind === "file" && tag.length === 5)
  ) {
    text(tag[1]);
    if (kind === "file") {
      text(tag[2]);
      integer(tag[3]);
    }
    const index = kind === "file" ? 4 : 2;
    return decode(tag[index], sourceAt, [...path, index]);
  }
  // Metadata cannot silently coerce dates, undefined, or nonfinite numbers.
  fail(`Unsupported archive tag ${kind}.`);
}
function validateCacheTag(value: unknown): void {
  const tag = array(value),
    kind = text(tag[0]);
  if (kind === "undefined" && tag.length === 1) return;
  if (
    kind === "number" &&
    tag.length === 2 &&
    ["-0", "NaN", "Infinity", "-Infinity"].includes(text(tag[1]))
  )
    return;
  if (
    kind === "date" &&
    tag.length === 2 &&
    Number.isFinite(Date.parse(text(tag[1])))
  )
    return;
  if (kind === "array" && tag.length === 2) {
    array(tag[1]).forEach(validateCacheTag);
    return;
  }
  if (kind === "object" && tag.length === 2) {
    const keys = new Set<string>();
    for (const value of array(tag[1])) {
      const pair = array(value),
        key = text(pair[0]);
      if (pair.length !== 2 || keys.has(key))
        fail("Invalid cache object field.");
      keys.add(key);
      validateCacheTag(pair[1]);
    }
    return;
  }
  decode(value);
}
function jsonValue(value: unknown): void {
  if (value === null || ["string", "number", "boolean"].includes(typeof value))
    return;
  if (Array.isArray(value)) {
    value.forEach(jsonValue);
    return;
  }
  const row = object(value);
  if (Object.hasOwn(row, "binary")) fail("Unsupported binary metadata.");
  Object.values(row).forEach(jsonValue);
}
function binary(value: unknown): ByteSource {
  const row = object(value) as unknown as Binary;
  if (!row.binary || typeof row.binary.read !== "function")
    fail("Missing archive bytes.");
  return row.binary;
}
function lazySource(
  reload: () => Promise<unknown[]>,
  path: number[],
  value: string,
): ByteSource {
  const bytes = encodedSource(value).bytes;
  return {
    bytes,
    async read(offset, length) {
      let current: unknown = await reload();
      for (const index of path) current = array(current)[index];
      const source = encodedSource(text(current));
      if (source.bytes !== bytes) fail("The archive byte source changed.");
      return source.read(offset, length);
    },
  };
}

/** Import only sealed sources. Native receipts, not browser markers, prove completion. */
export async function migrateNativeArchive(
  context: Pick<NativeServerContext, "serverId" | "workspaceId" | "request">,
  options: NativeArchiveOptions,
): Promise<NativeArchiveResult> {
  await options.verify();
  const archive = object(options.archive),
    proof = options.archiveProof;
  const owner =
    context.serverId === "local" ? "root" : `server:${context.serverId}`;
  if (
    !digestPattern.test(options.sourceIdentity) ||
    proof.owner !== owner ||
    proof.id !== archive.id ||
    !digestPattern.test(proof.sha256)
  )
    fail("The archive seal identity does not match.");
  let hashIndex = 0;
  async function hash(source: ByteSource, id: string): Promise<string> {
    const begin = object(
      await context.request("files.hashBegin", { id, bytes: source.bytes }),
    );
    if (begin.bytes !== source.bytes) fail("The native hash size differs.");
    if (begin.sha256 === null) {
      const size = integer(begin.chunkBytes);
      if (size === 0) fail("The native hash chunk size is invalid.");
      for (let offset = 0; offset < source.bytes; offset += size) {
        const length = Math.min(size, source.bytes - offset);
        const reply = object(
          await context.request("files.hashChunk", {
            id,
            offset,
            base64: base64(await source.read(offset, length)),
          }),
        );
        if (reply.nextOffset !== offset + length)
          fail("The native hash offset differs.");
      }
    }
    const result = object(await context.request("files.hashFinish", { id }));
    if (
      result.bytes !== source.bytes ||
      !digestPattern.test(text(result.sha256))
    )
      fail("The native hash receipt is invalid.");
    return text(result.sha256);
  }
  const sealHash = await hash(
    utf8(proof.serializedSnapshot),
    `as:${options.sourceIdentity}`,
  );
  if (sealHash !== proof.sha256) fail("The archive seal hash does not match.");
  const snapshot = object(JSON.parse(proof.serializedSnapshot));
  const profiles = array(archive.profiles).map((value) => {
      const row = object(value);
      return { owner: text(row.owner), snapshot: row.snapshot };
    }),
    selected = profiles.filter((row) => row.owner === owner);
  if (selected.length !== 1 || !equal(selected[0].snapshot, snapshot))
    fail("The archive snapshot identity differs.");
  const manifest: NativeArchiveManifest = {
    version: 1,
    archive: {
      id: proof.id,
      owner,
      sha256: proof.sha256,
      sourceIdentity: options.sourceIdentity,
    },
    excludedCaches: [],
  };
  if (options.legacyImportMarker) {
    const combined = combineBrowserProfiles(profiles);
    const combinedHash = await hash(
      utf8(JSON.stringify(combined)),
      `al:${options.sourceIdentity}`,
    );
    if (options.legacyImportMarker !== `${proof.id}:${combinedHash}:complete`)
      fail("The legacy archive marker identity differs.");
    manifest.legacyAdoption = {
      marker: options.legacyImportMarker,
      combinedSnapshotSha256: combinedHash,
    };
    await options.verify();
    return {
      mode: "adopt-current",
      metadata: [],
      bytes: [],
      archiveManifest: manifest,
    };
  }
  if (snapshot.version !== 1 && snapshot.version !== 2)
    fail("Unsupported archive version.");
  const rawPreferences = object(snapshot.preferences),
    logicalPreferences: Record<string, string> = Object.create(null),
    preferences: Record<string, string> = {};
  const prefix =
    context.serverId === "local" ? "" : `:server:${context.serverId}:`;
  for (const [key, value] of Object.entries(rawPreferences)) {
    text(value);
    if (prefix && !key.startsWith(prefix))
      fail("Unsupported foreign archive preference scope.");
    if (!prefix && key.startsWith(":server:"))
      fail("Unsupported foreign archive preferences.");
    const logical = key.slice(prefix.length);
    logicalPreferences[logical] = text(value);
    if (
      (logical.startsWith("codex-drafts:") &&
        logical !== `codex-drafts:${context.workspaceId}` &&
        !logical.startsWith(`codex-drafts:${context.workspaceId}:pending:`)) ||
      (logical.startsWith("codex-chat-draft:") &&
        !logical.startsWith(`codex-chat-draft:${context.workspaceId}:`)) ||
      (logical.startsWith("studio-native-draft-journal-v1:") &&
        !logical.startsWith(
          `studio-native-draft-journal-v1:${encodeURIComponent(context.workspaceId)}:`,
        ))
    )
      fail("Unsupported foreign draft workspace.");
    if (
      ![
        "studio-native-preferences-journal-v1",
        "studio-native-preferences-ready-v1",
        "studio-browser-profile-import-v1",
      ].includes(logical) &&
      !logical.startsWith("studio-native-draft-journal-v1:")
    )
      preferences[logical] = text(value);
  }
  const storage: Storage = {
    get length() {
      return Object.keys(logicalPreferences).length;
    },
    key: (i) => Object.keys(logicalPreferences)[i] ?? null,
    getItem: (key) =>
      logicalPreferences[key] === undefined ? null : logicalPreferences[key],
    setItem() {
      fail("Archive writes are forbidden.");
    },
    removeItem() {
      fail("Archive writes are forbidden.");
    },
    clear() {
      fail("Archive writes are forbidden.");
    },
  };
  const pendingPreferences = readPendingLegacyPreferences(storage).map(
    (write) => {
      const scope = `browser:${JSON.stringify(owner)}:`;
      return {
        ...write,
        params:
          write.method === "preferences.import"
            ? {
                ...write.params,
                entries: Object.fromEntries(
                  Object.entries(object(write.params.entries)).map(
                    ([key, value]) => [scope + key, value],
                  ),
                ),
              }
            : { ...write.params, key: scope + text(write.params.key) },
      };
    },
  );
  const pendingDrafts = new DraftJournal(storage, context.workspaceId)
    .load()
    .map((edit) => ({ workspace: context.workspaceId, ...edit }));
  const draftDocuments: unknown[] = [],
    outbox: unknown[] = [],
    monitorInputs: unknown[] = [],
    artifacts: Artifact[] = [];
  const uploads = new Map<string, ObjectValue>(),
    contents = new Map<string, ByteSource>(),
    recordings = new Map<string, ObjectValue>(),
    pcm = new Map<string, Map<number, ByteSource>>();
  const suffix =
    context.serverId === "local"
      ? ""
      : "server" +
        Array.from(new TextEncoder().encode(context.serverId), (b) =>
          b.toString(16).padStart(2, "0"),
        ).join("");
  const rxPrefix = `rxdb-dexie-studio${context.workspaceId}${suffix}--0--`,
    byteSuffix =
      context.serverId === "local" ? "" : `:server:${context.serverId}`;
  const seenDatabases = new Set<string>();
  // Keep at most one external source page. Byte descriptors retain paths, not pages.
  let activePage: { sha: string; records: unknown[] } | undefined;
  async function loadPage(sha: string): Promise<unknown[]> {
    if (activePage?.sha === sha) return activePage.records;
    const records = array(
      JSON.parse(text(await options.readChunk(owner, sha))),
    );
    activePage = { sha, records };
    return records;
  }
  for (const dbValue of array(snapshot.databases)) {
    const db = object(dbValue),
      name = text(db.name);
    if (seenDatabases.has(name)) fail("Duplicate archive database.");
    seenDatabases.add(name);
    const cacheParts = name.split("--"),
      cacheHash = cacheParts[0].slice(-64);
    const projectionCache =
      name === rxPrefix + "projections" ||
      (cacheParts.length === 3 &&
        /^[0-9]+$/.test(cacheParts[1]) &&
        digestPattern.test(cacheHash) &&
        cacheParts[0] ===
          `rxdb-dexie-${entityProjectionDatabaseName(context.workspaceId, cacheHash, suffix)}` &&
        (cacheParts[2] === "projections" ||
          cacheParts[2] === "_rxdb_internal" ||
          /^rx-replication-meta-[a-f0-9]{64}$/.test(cacheParts[2])));
    const known =
      projectionCache ||
      name === rxPrefix + "drafts" ||
      name === rxPrefix + "outbox" ||
      name === rxPrefix + "projections" ||
      name === "studio-monitor-input-journal-v1" ||
      name === "codex-studio-uploads" + byteSuffix ||
      name === "codex-studio-dictation" + byteSuffix;
    if (!known) fail(`Unsupported archive database ${name}.`);
    const seenStores = new Set<string>();
    for (const storeValue of array(db.stores)) {
      const store = object(storeValue),
        storeName = text(store.name);
      if (seenStores.has(storeName)) fail("Duplicate archive store.");
      seenStores.add(storeName);
      const expected =
        projectionCache || name.startsWith(rxPrefix)
          ? ["docs", "changes", "attachments"]
          : name === "studio-monitor-input-journal-v1"
            ? ["inputs"]
            : name.startsWith("codex-studio-uploads")
              ? ["uploads", "contents"]
              : ["recordings", "chunks"];
      if (!expected.includes(storeName))
        fail(`Unsupported archive store ${storeName}.`);
      async function* pages(): AsyncGenerator<{
        records: unknown[];
        reload?: () => Promise<unknown[]>;
      }> {
        if (snapshot.version === 1) {
          if (store.chunks !== undefined) fail("Invalid inline archive store.");
          yield { records: array(store.records) };
          return;
        }
        if (store.records !== undefined) fail("Invalid chunked archive store.");
        for (const referenceValue of array(store.chunks)) {
          const reference = object(referenceValue),
            sha = text(reference.sha256);
          if (reference.owner !== undefined && reference.owner !== owner)
            fail("The archive chunk owner differs.");
          if (!digestPattern.test(sha)) fail("Invalid archive chunk hash.");
          const serialized = text(await options.readChunk(owner, sha));
          if (
            new TextEncoder().encode(serialized).length !==
              integer(reference.bytes) ||
            (await hash(
              utf8(serialized),
              `ak:${options.sourceIdentity}:${hashIndex++}`,
            )) !== sha
          )
            fail("Archive chunk hash differs.");
          const records = array(JSON.parse(serialized));
          if (records.length !== integer(reference.records))
            fail("Archive chunk record count differs.");
          activePage = { sha, records };
          yield { records, reload: () => loadPage(sha) };
        }
      }
      let count = 0;
      const keys = new Set<string>();
      for await (const page of pages())
        for (const [recordIndex, recordValue] of page.records.entries()) {
          count++;
          const pair = array(recordValue);
          if (pair.length !== 2) fail("Invalid archive record.");
          const encodedKey = JSON.stringify(pair[0]);
          if (keys.has(encodedKey)) fail("Duplicate archive record key.");
          keys.add(encodedKey);
          if (projectionCache) {
            pair.forEach(validateCacheTag);
            continue;
          }
          const reload = page.reload;
          const sourceAt: SourceAt | undefined = reload
            ? (path, value) =>
                lazySource(reload, [recordIndex, 1, ...path], value)
            : undefined;
          const key = decode(pair[0]),
            row = decode(pair[1], sourceAt);
          if (name.startsWith(rxPrefix) && storeName !== "docs")
            fail("Unsupported nonempty draft or message auxiliary store.");
          const value = object(row);
          if (name === rxPrefix + "drafts" || name === rxPrefix + "outbox") {
            jsonValue(value);
            if (value.id !== key || typeof value.payload !== "string")
              fail("The saved document identity differs.");
            if (name === rxPrefix + "drafts") {
              integer(value.seq);
              if (![true, false, 0, 1].includes(value._deleted as boolean))
                fail("Invalid draft tombstone.");
              const saved = object(JSON.parse(value.payload));
              if (saved.id !== key) fail("The draft payload identity differs.");
              draftDocuments.push({
                id: key,
                payload: value.payload,
                seq: value.seq,
                _deleted: !!value._deleted,
              });
            } else {
              if (value._deleted) fail("Unsupported deleted outbox row.");
              const saved = object(JSON.parse(value.payload));
              if (object(saved.body).id !== key)
                fail("The outbox payload identity differs.");
              outbox.push({
                ...saved,
                id: key,
                workspace: context.workspaceId,
                displayPending: saved.displayPending ?? true,
                attachments: saved.attachments ?? [],
              });
            }
          } else if (name === "studio-monitor-input-journal-v1") {
            const parts = array(key);
            if (parts.length !== 2 || parts[0] !== context.serverId)
              fail("Unsupported foreign monitor input.");
            const entry = validMonitorInput(value);
            if (!entry) fail("Invalid archived monitor input.");
            monitorInputs.push({ monitorId: text(parts[1]), ...entry });
          } else if (name.startsWith("codex-studio-uploads")) {
            if (value.id !== key) fail("The upload identity differs.");
            if (storeName === "uploads") {
              const document = { ...value };
              delete document.bytes;
              delete document.file;
              jsonValue(document);
              if (document.workspace !== context.workspaceId)
                fail("Unsupported foreign upload workspace.");
              uploads.set(text(key), document);
              if (value.bytes !== undefined || value.file !== undefined)
                contents.set(text(key), binary(value.bytes ?? value.file));
            } else contents.set(text(key), binary(value.bytes ?? value.file));
          } else if (storeName === "recordings") {
            jsonValue(value);
            if (value.id !== key) fail("The recording identity differs.");
            recordings.set(text(key), value);
          } else {
            const parts = array(key);
            if (
              parts.length !== 2 ||
              parts[0] !== value.id ||
              parts[1] !== value.index ||
              object(value.pcm).view !== "Int16Array"
            )
              fail("Invalid recording chunk identity.");
            const source = binary(value.pcm);
            if (source.bytes % 2) fail("Invalid PCM16 archive chunk.");
            const chunks =
              pcm.get(text(value.id)) ?? new Map<number, ByteSource>();
            const index = integer(value.index);
            if (chunks.has(index)) fail("Duplicate recording chunk.");
            chunks.set(index, source);
            pcm.set(text(value.id), chunks);
          }
        }
      if (
        projectionCache ||
        (name.startsWith(rxPrefix) && storeName !== "docs")
      )
        manifest.excludedCaches.push({
          database: name,
          store: storeName,
          records: count,
          sha256: await hash(
            utf8(JSON.stringify(store)),
            `ac:${options.sourceIdentity}:${hashIndex++}`,
          ),
        });
    }
  }
  activePage = undefined;
  const versions = readLegacyDraftValues(
    context.workspaceId,
    storage,
    draftDocuments
      .map(object)
      .filter((row) => !row._deleted)
      .map((row) => JSON.parse(text(row.payload)) as DraftVersion),
  );
  const pendingVersions = versions.filter(
    (version) =>
      !draftDocuments
        .map(object)
        .some(
          (row) =>
            row.id === version.id &&
            equal(JSON.parse(text(row.payload)), version),
        ),
  );
  for (const [id, document] of uploads) {
    const source = contents.get(id);
    if (!source || source.bytes !== document.size)
      fail("The upload bytes are missing or differ.");
    contents.delete(id);
    artifacts.push({ kind: "upload", id, source, document });
  }
  if (contents.size) fail("Unsupported orphan upload bytes.");
  for (const [id, document] of recordings) {
    const chunks = pcm.get(id) ?? new Map<number, ByteSource>();
    const parts = [...chunks].sort(([a], [b]) => a - b);
    if (parts.some(([index], i) => index !== i))
      fail("The recording chunk interval is incomplete.");
    const source = joined(parts.map(([, part]) => part));
    if (source.bytes !== integer(document.samples) * 2)
      fail("The recording bytes are missing or differ.");
    pcm.delete(id);
    artifacts.push({ kind: "recording", id, source, document });
  }
  if (pcm.size) fail("Unsupported orphan recording bytes.");
  for (const [key, value] of Object.entries(preferences)) {
    const match = /^studio-upload-(cancelled|settled):(.+)$/.exec(key);
    if (match) {
      if (value !== "true") fail("Invalid upload settlement.");
      if (uploads.has(match[2])) fail("Conflicting active and settled upload.");
      artifacts.push({
        kind: "uploadTombstone",
        id: match[2],
        source: utf8(""),
        cancelled: match[1] === "cancelled",
      });
    }
  }
  artifacts.sort((a, b) =>
    a.kind < b.kind
      ? -1
      : a.kind > b.kind
        ? 1
        : a.id < b.id
          ? -1
          : a.id > b.id
            ? 1
            : 0,
  );
  if (new Set(artifacts.map((row) => row.id)).size !== artifacts.length)
    fail("Conflicting archived byte identities.");
  const expectedByteImports = artifacts.map(
    (_, index) => `archive-byte:v1:${options.sourceIdentity}:${index}`,
  );
  const entries = [];
  for (const [index, artifact] of artifacts.entries())
    entries.push({
      kind: artifact.kind,
      id: artifact.id,
      bytes: artifact.source.bytes,
      sha256: await hash(
        artifact.source,
        `af:${options.sourceIdentity}:${index}`,
      ),
      ...(artifact.document ? { document: artifact.document } : {}),
      ...(artifact.cancelled !== undefined
        ? { cancelled: artifact.cancelled }
        : {}),
    });
  await options.verify();
  const byteReceipts: NativeArchiveResult["bytes"] = [];
  for (const [index, artifact] of artifacts.entries()) {
    const entry = entries[index],
      manifestId = expectedByteImports[index];
    const byteManifest = {
      version: 1,
      id: manifestId,
      source: options.sourceIdentity,
      workspace: context.workspaceId,
      entries: [entry],
    };
    const begin = object(
      await context.request("files.importBegin", { manifest: byteManifest }),
    );
    if (
      begin.manifestId !== manifestId ||
      !digestPattern.test(text(begin.manifestHash)) ||
      !["pending", "complete"].includes(text(begin.status))
    )
      fail("The native byte import identity differs.");
    if (begin.status === "pending") {
      const size = integer(begin.chunkBytes);
      if (size === 0) fail("Invalid byte import chunk size.");
      for (let offset = 0; offset < artifact.source.bytes; offset += size) {
        const length = Math.min(size, artifact.source.bytes - offset);
        const reply = object(
          await context.request("files.importChunk", {
            manifestId,
            entry: 0,
            offset,
            base64: base64(await artifact.source.read(offset, length)),
          }),
        );
        if (reply.nextOffset !== offset + length)
          fail("The native byte import offset differs.");
      }
    }
    const receipt = object(
      await context.request("files.importCommit", { manifestId }),
    );
    const readback = await context.request("files.importReadback", {
      manifestId,
    });
    const saved = array(receipt.entries).map(object);
    if (
      !equal(receipt, readback) ||
      receipt.manifestId !== manifestId ||
      receipt.manifestHash !== begin.manifestHash ||
      receipt.source !== options.sourceIdentity ||
      receipt.workspace !== context.workspaceId ||
      saved.length !== 1 ||
      saved[0].id !== entry.id ||
      saved[0].kind !== entry.kind ||
      saved[0].bytes !== entry.bytes ||
      saved[0].sha256 !== entry.sha256 ||
      saved[0].cancelled !== (artifact.cancelled ?? null)
    )
      fail("The native byte readback receipt could not be verified.");
    byteReceipts.push({ manifestId, receipt });
  }
  const serializedSource = JSON.stringify({
    version: 1,
    sourceIdentity: options.sourceIdentity,
    expectedByteImports,
    archiveManifest: manifest,
    preferences,
    legacyPreferenceScope: owner,
    pendingPreferences,
    pendingDrafts,
    draftDocuments: draftDocuments.length
      ? [{ workspace: context.workspaceId, documents: draftDocuments }]
      : [],
    drafts: pendingVersions.length
      ? [
          {
            workspace: context.workspaceId,
            versions: pendingVersions,
            serializedParams: JSON.stringify({
              workspace: context.workspaceId,
              versions: pendingVersions,
            }),
          },
        ]
      : [],
    monitorInputs,
    outbox,
  });
  const sourceSha256 = await hash(
      utf8(serializedSource),
      `am:${options.sourceIdentity}`,
    ),
    manifestId = `archive-metadata:v1:${options.sourceIdentity}`;
  await options.verify();
  const receipt = object(
    await context.request("client_storage.import", {
      manifestId,
      serializedSource,
    }),
  );
  const readback = await context.request("client_storage.status", {
    manifestId,
  });
  if (
    !equal(receipt, readback) ||
    receipt.manifestId !== manifestId ||
    receipt.sourceSha256 !== sourceSha256 ||
    receipt.imported !== true ||
    receipt.sourceIdentity !== options.sourceIdentity ||
    !equal(receipt.expectedByteImports, expectedByteImports) ||
    !equal(receipt.archiveManifest, manifest)
  )
    fail("The native metadata readback receipt could not be verified.");
  return {
    mode: "direct",
    metadata: [{ manifestId, sourceSha256 }],
    bytes: byteReceipts,
    archiveManifest: manifest,
  };
}
