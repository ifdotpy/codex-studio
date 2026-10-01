export type LocalDraftRecord = {
  version: 1;
  workspace: string;
  session: string;
  text: string;
  deleted: boolean;
  source: "legacy" | "local" | "remote";
  /** Read-only compatibility for records written before compact checkpoints. */
  legacyBaseline?: string | null;
  /** SHA-256 of the session-bound exact UTF-16 value last observed in the legacy map. */
  legacyBaselineHash?: string;
  /** New old-tab chat awaiting its first durable journal version. */
  legacyPending?: boolean;
  /** Last aggregate-map branch version journaled by this client. */
  legacyUpdated?: number;
  /** Writer version that produced this local/remote value, when known. */
  updated?: number;
};

export type DraftMap = Record<string, string>;

const localPrefix = "codex-chat-draft:";

const sha256RoundConstants = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
  0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
  0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
  0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
  0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
  0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
  0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
  0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

function rotateRight(value: number, count: number) {
  return (value >>> count) | (value << (32 - count));
}

function sha256(bytes: Uint8Array) {
  const paddedLength = Math.ceil((bytes.length + 9) / 64) * 64;
  const padded = new Uint8Array(paddedLength);
  padded.set(bytes);
  padded[bytes.length] = 0x80;
  const bitLength = bytes.length * 8;
  const view = new DataView(padded.buffer);
  view.setUint32(paddedLength - 8, Math.floor(bitLength / 0x100000000));
  view.setUint32(paddedLength - 4, bitLength >>> 0);

  const state = new Uint32Array([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
    0x1f83d9ab, 0x5be0cd19,
  ]);
  const words = new Uint32Array(64);
  for (let offset = 0; offset < paddedLength; offset += 64) {
    for (let index = 0; index < 16; index++)
      words[index] = view.getUint32(offset + index * 4);
    for (let index = 16; index < 64; index++) {
      const x = words[index - 15];
      const y = words[index - 2];
      const sigma0 = rotateRight(x, 7) ^ rotateRight(x, 18) ^ (x >>> 3);
      const sigma1 = rotateRight(y, 17) ^ rotateRight(y, 19) ^ (y >>> 10);
      words[index] =
        (words[index - 16] + sigma0 + words[index - 7] + sigma1) >>> 0;
    }
    let [a, b, c, d, e, f, g, h] = state;
    for (let index = 0; index < 64; index++) {
      const sum1 = rotateRight(e, 6) ^ rotateRight(e, 11) ^ rotateRight(e, 25);
      const choice = (e & f) ^ (~e & g);
      const t1 =
        (h + sum1 + choice + sha256RoundConstants[index] + words[index]) >>> 0;
      const sum0 = rotateRight(a, 2) ^ rotateRight(a, 13) ^ rotateRight(a, 22);
      const majority = (a & b) ^ (a & c) ^ (b & c);
      const t2 = (sum0 + majority) >>> 0;
      h = g;
      g = f;
      f = e;
      e = (d + t1) >>> 0;
      d = c;
      c = b;
      b = a;
      a = (t1 + t2) >>> 0;
    }
    state[0] = (state[0] + a) >>> 0;
    state[1] = (state[1] + b) >>> 0;
    state[2] = (state[2] + c) >>> 0;
    state[3] = (state[3] + d) >>> 0;
    state[4] = (state[4] + e) >>> 0;
    state[5] = (state[5] + f) >>> 0;
    state[6] = (state[6] + g) >>> 0;
    state[7] = (state[7] + h) >>> 0;
  }
  return [...state].map((part) => part.toString(16).padStart(8, "0")).join("");
}

/** A synchronous SHA-256 over the exact session and UTF-16 draft code units. */
export function legacyBaselineHash(session: string, value: string | null) {
  const byteLength =
    6 + session.length * 2 + (value === null ? 0 : value.length * 2);
  const bytes = new Uint8Array(byteLength);
  const view = new DataView(bytes.buffer);
  bytes[0] = 1; // Checkpoint encoding version.
  view.setUint32(1, session.length);
  let offset = 5;
  for (let index = 0; index < session.length; index++, offset += 2)
    view.setUint16(offset, session.charCodeAt(index));
  bytes[offset++] = value === null ? 0 : 1;
  if (value !== null)
    for (let index = 0; index < value.length; index++, offset += 2)
      view.setUint16(offset, value.charCodeAt(index));
  return sha256(bytes);
}

function checkpointOf(record: LocalDraftRecord, session: string) {
  if (record.legacyBaselineHash !== undefined) return record.legacyBaselineHash;
  return record.legacyBaseline === undefined
    ? undefined
    : legacyBaselineHash(session, record.legacyBaseline);
}

export function localDraftKey(scope: string, session: string) {
  return `${localPrefix}${scope.slice("codex-drafts:".length)}:${encodeURIComponent(session)}`;
}

function workspaceOf(scope: string) {
  return scope.slice("codex-drafts:".length);
}

function parseRecord(raw: string, workspace: string, session: string) {
  const record = JSON.parse(raw) as LocalDraftRecord;
  if (
    !record ||
    record.version !== 1 ||
    record.workspace !== workspace ||
    record.session !== session ||
    typeof record.text !== "string" ||
    typeof record.deleted !== "boolean" ||
    !["legacy", "local", "remote"].includes(record.source) ||
    (record.legacyBaseline !== undefined &&
      record.legacyBaseline !== null &&
      typeof record.legacyBaseline !== "string") ||
    (record.legacyBaselineHash !== undefined &&
      (typeof record.legacyBaselineHash !== "string" ||
        !/^[0-9a-f]{64}$/.test(record.legacyBaselineHash))) ||
    (record.legacyPending !== undefined &&
      typeof record.legacyPending !== "boolean") ||
    (record.legacyUpdated !== undefined &&
      !Number.isFinite(record.legacyUpdated)) ||
    (record.updated !== undefined && !Number.isFinite(record.updated))
  )
    throw new Error("A saved draft could not be read. Keep this chat open.");
  return record;
}

export function readLocalDraftRecord(scope: string, session: string) {
  const raw = localStorage.getItem(localDraftKey(scope, session));
  return raw === null ? null : parseRecord(raw, workspaceOf(scope), session);
}

export function readLocalDrafts(
  scope: string,
  onCorrupt: () => void = () => {},
): DraftMap {
  const drafts: DraftMap = {};
  const workspace = workspaceOf(scope);
  for (let index = 0; index < localStorage.length; index++) {
    const key = localStorage.key(index)!;
    if (!key.startsWith(`${localPrefix}${workspace}:`)) continue;
    try {
      const session = decodeURIComponent(
        key.slice(`${localPrefix}${workspace}:`.length),
      );
      const record = parseRecord(
        localStorage.getItem(key)!,
        workspace,
        session,
      );
      if (!record.deleted) drafts[session] = record.text;
    } catch {
      onCorrupt();
    }
  }
  return drafts;
}

export function readDraftsWithLegacyFallback(
  scope: string,
  onCorrupt: () => void = () => {},
): DraftMap {
  const drafts = readLocalDrafts(scope, onCorrupt);
  const raw = localStorage.getItem(scope);
  if (raw === null) return drafts;
  let legacy: DraftMap;
  try {
    legacy = JSON.parse(raw) as DraftMap;
    if (!legacy || typeof legacy !== "object" || Array.isArray(legacy))
      throw new Error("Invalid legacy draft map");
  } catch {
    onCorrupt();
    return drafts;
  }
  for (const [session, text] of Object.entries(legacy)) {
    if (typeof text !== "string") continue;
    try {
      if (readLocalDraftRecord(scope, session) === null) drafts[session] = text;
    } catch {
      onCorrupt();
      drafts[session] = text;
    }
  }
  return drafts;
}

export function writeLocalDraftRecord(
  scope: string,
  session: string,
  text: string | null,
  source: LocalDraftRecord["source"] = "local",
  metadata: Pick<
    LocalDraftRecord,
    | "legacyBaseline"
    | "legacyBaselineHash"
    | "legacyPending"
    | "legacyUpdated"
    | "updated"
  > = {},
) {
  const key = localDraftKey(scope, session);
  const previousRaw = localStorage.getItem(key);
  const previous = previousRaw
    ? parseRecord(previousRaw, workspaceOf(scope), session)
    : null;
  const preserved: Partial<LocalDraftRecord> = previous ? { ...previous } : {};
  delete preserved.legacyBaseline;
  const baselineHash =
    metadata.legacyBaselineHash ??
    (metadata.legacyBaseline !== undefined
      ? legacyBaselineHash(session, metadata.legacyBaseline)
      : checkpointOf(previous || ({} as LocalDraftRecord), session));
  const pending = metadata.legacyPending ?? previous?.legacyPending;
  const record: LocalDraftRecord = {
    ...preserved,
    version: 1,
    workspace: workspaceOf(scope),
    session,
    text: text ?? "",
    deleted: text === null,
    source,
    ...metadata,
  };
  delete record.legacyBaseline;
  if (baselineHash !== undefined) record.legacyBaselineHash = baselineHash;
  if (pending !== undefined) record.legacyPending = pending;
  else delete record.legacyPending;
  localStorage.setItem(key, JSON.stringify(record));
}

export function migrateLegacyDrafts(
  scope: string,
  onCorrupt: () => void = () => {},
  options: { includeNewLegacyChats?: boolean } = {},
) {
  const raw = localStorage.getItem(scope);
  const legacy = (raw === null ? {} : JSON.parse(raw)) as DraftMap;
  if (!legacy || typeof legacy !== "object" || Array.isArray(legacy))
    throw new Error("A saved draft could not be read. Keep this chat open.");
  const workspace = workspaceOf(scope);
  const imported: Record<string, string | null> = {};
  for (const [session, value] of Object.entries(legacy)) {
    if (typeof value !== "string") continue;
    let existing: LocalDraftRecord | null;
    try {
      const key = localDraftKey(scope, session);
      const existingRaw = localStorage.getItem(key);
      if (existingRaw === null) {
        existing = null;
      } else {
        existing = parseRecord(existingRaw, workspace, session);
      }
    } catch {
      onCorrupt();
      continue;
    }
    if (!existing) {
      writeLocalDraftRecord(
        scope,
        session,
        value,
        "legacy",
        options.includeNewLegacyChats
          ? { legacyPending: true }
          : { legacyBaselineHash: legacyBaselineHash(session, value) },
      );
      if (options.includeNewLegacyChats) imported[session] = value;
      continue;
    }
    if (existing.legacyPending) {
      imported[session] = value;
      continue;
    }
    let baselineHash = existing.legacyBaselineHash;
    if (baselineHash === undefined) {
      const baseline =
        existing.legacyBaseline !== undefined
          ? existing.legacyBaseline
          : existing.source === "legacy"
            ? existing.text
            : value;
      baselineHash = legacyBaselineHash(session, baseline);
      writeLocalDraftRecord(
        scope,
        session,
        existing.deleted ? null : existing.text,
        existing.source,
        { legacyBaselineHash: baselineHash },
      );
    }
    if (baselineHash === legacyBaselineHash(session, value)) continue;
    // Leave the durable winner untouched until the independent legacy
    // version has been journaled by the hook. A failed journal write then
    // leaves the old baseline available for retry.
    imported[session] = value;
  }
  const workspacePrefix = `${localPrefix}${workspace}:`;
  for (let index = 0; index < localStorage.length; index++) {
    const key = localStorage.key(index)!;
    if (!key.startsWith(workspacePrefix)) continue;
    let session: string;
    let existing: LocalDraftRecord;
    try {
      session = decodeURIComponent(key.slice(workspacePrefix.length));
      if (Object.hasOwn(legacy, session)) continue;
      existing = parseRecord(localStorage.getItem(key)!, workspace, session);
    } catch {
      onCorrupt();
      continue;
    }
    if (existing.legacyPending) {
      imported[session] = null;
      continue;
    }
    let baselineHash = existing.legacyBaselineHash;
    if (baselineHash === undefined) {
      const baseline =
        existing.legacyBaseline !== undefined
          ? existing.legacyBaseline
          : existing.source === "legacy"
            ? existing.text
            : null;
      baselineHash = legacyBaselineHash(session, baseline);
      writeLocalDraftRecord(
        scope,
        session,
        existing.deleted ? null : existing.text,
        existing.source,
        { legacyBaselineHash: baselineHash },
      );
    }
    if (baselineHash === legacyBaselineHash(session, null)) continue;
    imported[session] = null;
  }
  return imported;
}

export function copyLocalDraftScope(
  from: string,
  to: string,
  onCorrupt: () => void = () => {},
) {
  const sourceWorkspace = workspaceOf(from);
  const destinationWorkspace = workspaceOf(to);
  for (let index = 0; index < localStorage.length; index++) {
    const key = localStorage.key(index)!;
    if (!key.startsWith(`${localPrefix}${sourceWorkspace}:`)) continue;
    let session: string;
    let record: LocalDraftRecord;
    try {
      session = decodeURIComponent(
        key.slice(`${localPrefix}${sourceWorkspace}:`.length),
      );
      record = parseRecord(
        localStorage.getItem(key)!,
        sourceWorkspace,
        session,
      );
    } catch {
      onCorrupt();
      continue;
    }
    const destination = localDraftKey(to, session);
    if (localStorage.getItem(destination) !== null) continue;
    const compacted = { ...record };
    delete compacted.legacyBaseline;
    if (
      compacted.legacyBaselineHash === undefined &&
      record.legacyBaseline !== undefined
    )
      compacted.legacyBaselineHash = legacyBaselineHash(
        session,
        record.legacyBaseline,
      );
    localStorage.setItem(
      destination,
      JSON.stringify({
        ...compacted,
        workspace: destinationWorkspace,
        source: record.source === "legacy" ? "local" : record.source,
      }),
    );
  }
}
