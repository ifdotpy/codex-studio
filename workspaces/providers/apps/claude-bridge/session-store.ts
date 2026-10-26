import type { Options } from "@anthropic-ai/claude-agent-sdk";
import type {
  Session,
  SessionMetadata,
  JournalRepairReport,
} from "./session-state.ts";
import type { HistoryReadOptions } from "./history-index.ts";
import type { JournalChange } from "./journal-repair.ts";
import { object } from "./history-records.ts";
export interface StoreOptions {
  maxCacheBytes?: number;
  compactBytes?: number;
  compactRecords?: number;
  diskFullRetryMs?: number;
  isPinned?: (id: string) => boolean;
  onSaved?: (session: Session) => void;
  nativeHistory?: HistoryReadOptions;
}
interface SavedSession {
  session: Session;
  sequence: number;
  records: number;
  journalBytes: number;
}
interface CacheEntry extends SavedSession {
  shadow: Session;
  used: number;
  syncSessionBytesOnWrite: boolean;
  metadataDirty?: boolean;
  journalRepairNeeded?: boolean;
  sessionBytes: number;
  shadowBytes: number;
  bytes: number;
}
import fs from "node:fs/promises";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { reconcileNativeSession } from "./session-identity.ts";
import { reconcileRestartTurns } from "./restart-reconcile.ts";
import { validJournalRecord, nativeConfirmsJournal } from "./journal-repair.ts";

// These documents are written and read by this bridge; preserve the existing
// trusted local persistence boundary without adding migration-time validation.
function parseSessionSnapshot(
  text: string,
): Session & { format?: string; session: Session; sequence: number } {
  return JSON.parse(text);
}
function parseSessionMetadata(text: string): SessionMetadata {
  return JSON.parse(text);
}

const FORMAT = "studio-claude-session-v1";
// Maximum bytes read from a saved repair marker. Never read journal or transcript text here.
const MAX_REPAIR_REPORT_BYTES = 16 * 1024;
const DEFAULT_CACHE_BYTES = 64 * 1024 * 1024;
const DEFAULT_COMPACT_BYTES = 1024 * 1024;
const DEFAULT_COMPACT_RECORDS = 128;
// Milliseconds between save attempts for a session whose write failed because the disk is full.
const DEFAULT_DISK_FULL_RETRY_MS = 15_000;

// A write failed because the volume has no free space (ENOSPC) or the user quota is used (EDQUOT).
export function isDiskFull(error: unknown): boolean {
  for (
    let current: unknown = error, depth = 0;
    object(current) && depth < 8;
    depth++
  ) {
    if (current.code === "ENOSPC" || current.code === "EDQUOT") return true;
    if (Array.isArray(current.errors) && current.errors.some(isDiskFull))
      return true;
    current = current.cause;
  }
  return false;
}

// Replace a file in one rename. A failed write or rename removes the
// temporary file, so the previous file stays and no partial file remains.
export async function writeFileAtomic(
  file: string,
  data: Parameters<typeof fs.writeFile>[1],
  mode = 0o600,
) {
  const tmp = file + "." + randomUUID() + ".tmp";
  try {
    await fs.writeFile(tmp, data, { mode });
    await fs.rename(tmp, file);
  } catch (error) {
    await fs.rm(tmp, { force: true }).catch(() => {});
    throw error;
  }
}

export function ephemeralSessionOptions(
  session?: Pick<Session, "ephemeral">,
): Partial<Options> {
  if (session?.ephemeral !== true) return {};
  return {
    persistSession: false,
    permissionMode: "dontAsk",
    tools: [],
    allowedTools: [],
    settingSources: [],
    strictMcpConfig: true,
    mcpServers: {},
  };
}

function copy<T>(value: T): T {
  return structuredClone(value);
}

function estimateJsonBytes(value: unknown): number {
  if (value === undefined) return 0;
  if (value === null) return 4;
  if (typeof value === "string")
    return Buffer.byteLength(JSON.stringify(value));
  if (typeof value === "number") return Buffer.byteLength(String(value));
  if (typeof value === "boolean") return value ? 4 : 5;
  if (Array.isArray(value))
    return (
      2 +
      value.reduce(
        (sum, item) =>
          sum + estimateJsonBytes(item === undefined ? null : item),
        0,
      ) +
      Math.max(0, value.length - 1)
    );
  const entries = Object.entries(value ?? {}).filter(
    ([, item]) => item !== undefined,
  );
  return (
    2 +
    entries.reduce(
      (sum, [key, item]) =>
        sum +
        Buffer.byteLength(JSON.stringify(key)) +
        1 +
        estimateJsonBytes(item),
      0,
    ) +
    Math.max(0, entries.length - 1)
  );
}

function diff(
  before: unknown,
  after: unknown,
  location: JournalChange["path"] = [],
  changes: JournalChange[] = [],
): number {
  if (before === after) return 0;
  if (
    before == null ||
    after == null ||
    typeof before !== "object" ||
    typeof after !== "object"
  ) {
    changes.push({ op: "set", path: location, value: after });
    return estimateJsonBytes(after) - estimateJsonBytes(before);
  }
  if (Array.isArray(before) && Array.isArray(after)) {
    const shared = Math.min(before.length, after.length);
    if (after.length < before.length) {
      changes.push({ op: "set", path: location, value: after });
      return estimateJsonBytes(after) - estimateJsonBytes(before);
    }
    let delta = 0;
    for (let i = 0; i < shared; i++)
      delta += diff(before[i], after[i], [...location, i], changes);
    if (after.length > before.length) {
      changes.push({
        op: "append",
        path: location,
        value: after.slice(before.length),
      });
      delta += after
        .slice(before.length)
        .reduce((sum, value) => sum + estimateJsonBytes(value), 0);
      delta += after.length - before.length - (before.length === 0 ? 1 : 0);
    }
    return delta;
  }
  if (
    before &&
    after &&
    typeof before === "object" &&
    typeof after === "object" &&
    object(before) &&
    object(after)
  ) {
    const oldKeys = Object.keys(before).filter(
      (key) => before[key] !== undefined,
    );
    const newKeys = Object.keys(after).filter(
      (key) => after[key] !== undefined,
    );
    let delta =
      Math.max(0, newKeys.length - 1) - Math.max(0, oldKeys.length - 1);
    for (const key of oldKeys) {
      if (!(key in after) || after[key] === undefined) {
        changes.push({ op: "delete", path: [...location, key] });
        delta -=
          Buffer.byteLength(JSON.stringify(key)) +
          1 +
          estimateJsonBytes(before[key]);
      } else
        delta += diff(before[key], after[key], [...location, key], changes);
    }
    for (const key of newKeys) {
      if (key in before && before[key] !== undefined) continue;
      changes.push({ op: "set", path: [...location, key], value: after[key] });
      delta +=
        Buffer.byteLength(JSON.stringify(key)) +
        1 +
        estimateJsonBytes(after[key]);
    }
    return delta;
  }
  changes.push({ op: "set", path: location, value: after });
  return estimateJsonBytes(after) - estimateJsonBytes(before);
}

function replaySessionJournal(
  session: Session,
  changes: JournalChange[],
): Session {
  let result: unknown = session;
  for (const change of changes) {
    let parent: unknown = result;
    for (const key of change.path.slice(0, -1)) {
      if (parent === null || typeof parent !== "object")
        throw new TypeError("Invalid journal parent");
      parent = Reflect.get(parent, key);
    }
    const key = change.path.at(-1);
    if (change.op === "set" && !change.path.length) {
      result = change.value;
      continue;
    }
    if (parent === null || typeof parent !== "object" || key === undefined)
      throw new TypeError("Invalid journal path");
    if (change.op === "append") {
      const target: unknown = Reflect.get(parent, key);
      if (!Array.isArray(target) || !Array.isArray(change.value))
        throw new TypeError("Invalid journal append");
      target.push(...change.value);
    } else if (change.op === "delete") Reflect.deleteProperty(parent, key);
    else Reflect.set(parent, key, change.value);
  }
  // Trust boundary: this bridge wrote these journal records and snapshots.
  // Schema validation is deliberately outside this behavior-preserving migration.
  return result as Session;
}

export function createSessionStore(root: string, options: StoreOptions = {}) {
  const directory = path.join(root, "sessions");
  const ready = fs.mkdir(directory, { recursive: true, mode: 0o700 });
  const configuredCacheBytes = Number(process.env.STUDIO_CLAUDE_CACHE_BYTES);
  const maxCacheBytes =
    options.maxCacheBytes ??
    (Number.isFinite(configuredCacheBytes) && configuredCacheBytes > 0
      ? configuredCacheBytes
      : DEFAULT_CACHE_BYTES);
  const compactBytes = options.compactBytes ?? DEFAULT_COMPACT_BYTES;
  const compactRecords = options.compactRecords ?? DEFAULT_COMPACT_RECORDS;
  const configuredRetryMs = Number(
    process.env.STUDIO_CLAUDE_DISK_FULL_RETRY_MS,
  );
  const diskFullRetryMs =
    options.diskFullRetryMs ??
    (Number.isFinite(configuredRetryMs) && configuredRetryMs > 0
      ? configuredRetryMs
      : DEFAULT_DISK_FULL_RETRY_MS);
  // Sessions whose last write failed because the disk is full. They stay cached
  // and are saved again from the live object until a write succeeds.
  const unsaved = new Map<string, Session>();
  let unsavedTimer: NodeJS.Timeout | null = null;
  const sessions = new Map<string, Session>();
  const entries = new Map<string, CacheEntry>();
  const writes = new Map<string, Promise<void>>();
  // Per session, the save that waits behind the running one and has not started.
  const queued = new Map<
    string,
    { session: Session; promise: Promise<void> }
  >();
  const loads = new Map<string, Promise<Session>>();
  const evicted = new Map<string, WeakRef<Session>>();
  const metadataCache = new Map<string, SessionMetadata>();
  const finalizer = new FinalizationRegistry<{
    id: string;
    ref: WeakRef<Session>;
  }>(({ id, ref }) => {
    if (evicted.get(id) === ref) evicted.delete(id);
  });
  let clock = 0;
  let retainedBytes = 0;

  const fileFor = (id: string) => {
    if (!/^[a-f0-9-]{36}$/.test(id))
      throw new Error("Invalid Claude session identity");
    return path.join(directory, id + ".json");
  };
  const logFor = (id: string) => fileFor(id).replace(/\.json$/, ".jsonl");
  const metadataFileFor = (id: string) =>
    fileFor(id).replace(/\.json$/, ".meta.json");
  const metadataFor = (
    session: Session,
    revision: string,
  ): SessionMetadata => ({
    id: session.id,
    cwd: session.cwd,
    createdAt: session.createdAt,
    updatedAt: session.updatedAt || session.createdAt,
    preview: session.preview || "",
    name: session.name ?? null,
    model: session.model,
    approvalPolicy: session.approvalPolicy ?? null,
    activePermissionProfile: session.activePermissionProfile ?? null,
    revision,
  });
  const writeMetadata = async (
    session: Session,
    revision: string = randomUUID(),
  ) => {
    const metadata = metadataFor(session, revision);
    await writeFileAtomic(
      metadataFileFor(session.id),
      JSON.stringify(metadata),
    );
    metadataCache.set(session.id, metadata);
    return metadata;
  };
  const readBase = async (id: string): Promise<SavedSession> => {
    await ready;
    const raw = await fs.readFile(fileFor(id), "utf8");
    const saved = parseSessionSnapshot(raw);
    if (saved?.format === FORMAT)
      return {
        session: saved.session,
        sequence: saved.sequence,
        records: 0,
        journalBytes: 0,
      };
    return { session: saved, sequence: 0, records: 0, journalBytes: 0 };
  };
  const readAll = async (id: string): Promise<SavedSession> => {
    const base = await readBase(id);
    let logBytes = Buffer.alloc(0);
    try {
      logBytes = await fs.readFile(logFor(id));
    } catch (error) {
      if (!object(error) || error.code !== "ENOENT") throw error;
    }
    const log = logBytes.toString("utf8");
    const completeLines = log.split("\n");
    const tornTail = completeLines.at(-1) !== "";
    if (tornTail) completeLines.pop();
    const validLogBytes = tornTail
      ? logBytes.lastIndexOf(0x0a) + 1
      : logBytes.length;
    if (tornTail) await fs.truncate(logFor(id), validLogBytes);
    const originalScope = [
      base.session.id,
      base.session.nativeId,
      base.session.cwd,
      base.session.nativeHistoryCwd,
    ];
    let records = 0,
      skippedRows = 0,
      invalidSequence = false,
      malformedTail = false;
    const snapshotSequence = base.sequence;
    for (const line of completeLines) {
      if (!line) continue;
      let record: unknown;
      try {
        record = JSON.parse(line);
      } catch {
        skippedRows++;
        malformedTail = true;
        continue;
      }
      if (!validJournalRecord(record)) {
        skippedRows++;
        invalidSequence = true;
        continue;
      }
      if (record.sequence >= snapshotSequence) malformedTail = false;
      if (record.sequence <= snapshotSequence) continue;
      if (record.sequence !== base.sequence + 1) invalidSequence = true;
      try {
        base.session = replaySessionJournal(base.session, record.changes);
      } catch {
        skippedRows++;
        invalidSequence = true;
        continue;
      }
      base.sequence = record.sequence;
      records++;
    }
    // A complete corrupt final row can hide settings or a submitted control
    // operation. Native message text cannot prove that state was never written.
    invalidSequence ||= malformedTail;
    base.records = records;
    base.journalBytes = validLogBytes;
    if (skippedRows || invalidSequence) {
      const marker = fileFor(id).replace(/\.json$/, ".repair.json");
      const report: JournalRepairReport = {
        status: "needs_repair",
        skippedRows,
        invalidSequence,
        scopeConfirmed: false,
        nativeHistoryConfirmed: false,
      };
      const sameScope =
        JSON.stringify(originalScope) ===
        JSON.stringify([
          base.session?.id,
          base.session?.nativeId,
          base.session?.cwd,
          base.session?.nativeHistoryCwd,
        ]);
      const confirmed =
        sameScope &&
        !invalidSequence &&
        (await nativeConfirmsJournal(base.session, options.nativeHistory).catch(
          () => false,
        ));
      report.scopeConfirmed = sameScope;
      report.nativeHistoryConfirmed = Boolean(confirmed);
      await writeFileAtomic(marker, JSON.stringify(report));
      if (!confirmed)
        throw Object.assign(
          new Error(
            `Claude session ${id} needs repair: ${skippedRows} malformed journal rows; native history cannot confirm the saved state.`,
          ),
          { code: "CLAUDE_SESSION_REPAIR_REQUIRED", data: report },
        );
      // Preserve the original bytes before replacing the snapshot and journal.
      await writeFileAtomic(logFor(id) + ".corrupt", logBytes);
      base.session.journalRepair = { ...report, status: "repaired" };
      await atomicSnapshot(id, base.session, base.sequence);
      await fs.writeFile(logFor(id), "", { mode: 0o600 });
      await writeFileAtomic(marker, JSON.stringify(base.session.journalRepair));
      base.records = 0;
      base.journalBytes = 0;
    }
    return base;
  };
  // Measure both retained objects only when the cache adopts or creates an entry.
  const measure = (entry: CacheEntry) => {
    entry.sessionBytes = estimateJsonBytes(entry.session);
    entry.shadowBytes = estimateJsonBytes(entry.shadow);
    entry.bytes = entry.sessionBytes + entry.shadowBytes;
  };
  const rememberEvicted = (id: string, session: Session) => {
    const ref = new WeakRef(session);
    evicted.set(id, ref);
    finalizer.unregister(session);
    finalizer.register(session, { id, ref }, session);
  };
  const adopt = (id: string, session: Session, saved: SavedSession) => {
    const entry: CacheEntry = {
      sessionBytes: 0,
      shadowBytes: 0,
      bytes: 0,
      session,
      shadow: saved.session,
      sequence: saved.sequence,
      records: saved.records || 0,
      journalBytes: saved.journalBytes || 0,
      used: ++clock,
      syncSessionBytesOnWrite: true,
      metadataDirty: true,
      journalRepairNeeded: false,
    };
    measure(entry);
    sessions.set(id, session);
    entries.set(id, entry);
    retainedBytes += entry.bytes;
    const ref = evicted.get(id);
    if (ref?.deref() === session) {
      evicted.delete(id);
      finalizer.unregister(session);
    }
    enforceLimit();
    return session;
  };
  const enforceLimit = () => {
    while (retainedBytes > maxCacheBytes) {
      let candidate: [string, CacheEntry] | undefined;
      for (const [id, entry] of entries) {
        if (options.isPinned?.(id) || writes.has(id) || unsaved.has(id))
          continue;
        if (!candidate || entry.used < candidate[1].used)
          candidate = [id, entry];
      }
      if (!candidate) break;
      const [id, entry] = candidate;
      rememberEvicted(id, entry.session);
      sessions.delete(id);
      entries.delete(id);
      retainedBytes -= entry.bytes;
    }
  };
  const remember = (
    session: Session,
    sequence = 0,
    records = 0,
    journalBytes = 0,
    shadow = copy(session),
    syncSessionBytesOnWrite = false,
  ) => {
    const id = session.id;
    sessions.set(id, session);
    const previous = entries.get(id);
    if (previous) retainedBytes -= previous.bytes;
    const entry: CacheEntry = {
      sessionBytes: 0,
      shadowBytes: 0,
      bytes: 0,
      session,
      shadow: previous?.shadow || shadow,
      sequence: previous?.sequence ?? sequence,
      records: previous?.records ?? records,
      journalBytes: previous?.journalBytes ?? journalBytes,
      used: ++clock,
      syncSessionBytesOnWrite,
    };
    measure(entry);
    entries.set(id, entry);
    retainedBytes += entry.bytes;
    enforceLimit();
    return session;
  };
  const atomicSnapshot = (id: string, session: Session, sequence: number) =>
    writeFileAtomic(
      fileFor(id),
      JSON.stringify({ format: FORMAT, sequence, session }),
    );
  const persistNow = async (session: Session) => {
    await ready;
    const id = session.id;
    if (loads.has(id) && (await loads.get(id)) !== session)
      throw new Error("Claude session identity conflict during load");
    const cached = sessions.get(id);
    if (cached && cached !== session)
      throw new Error("Claude session object is not the cached identity");
    const remembered = evicted.get(id)?.deref();
    if (remembered && remembered !== session && !cached)
      throw new Error(
        "Claude session object conflicts with a live evicted identity",
      );
    let entry = entries.get(id);
    if (!entry) {
      let saved;
      try {
        saved = await readAll(id);
      } catch (error) {
        if (!object(error) || error.code !== "ENOENT") throw error;
        const initial = copy(session);
        await atomicSnapshot(id, initial, 0);
        await fs.writeFile(logFor(id), "", { mode: 0o600 });
        remember(session, 0, 0, 0, initial, true);
        const created = entries.get(id);
        if (created) created.metadataDirty = true;
        await writeMetadata(session);
        if (created) created.metadataDirty = false;
        return;
      }
      entry = {
        sessionBytes: 0,
        shadowBytes: 0,
        bytes: 0,
        session,
        shadow: saved.session,
        sequence: saved.sequence,
        records: saved.records || 0,
        journalBytes: saved.journalBytes || 0,
        used: ++clock,
        syncSessionBytesOnWrite: true,
        metadataDirty: true,
        journalRepairNeeded: false,
      };
      measure(entry);
      sessions.set(id, session);
      entries.set(id, entry);
      retainedBytes += entry.bytes;
      const ref = evicted.get(id);
      if (ref?.deref() === session) {
        evicted.delete(id);
        finalizer.unregister(session);
      }
      enforceLimit();
    }
    if (entry.session !== session)
      throw new Error("Claude session object is not the cached identity");
    const journalFile = logFor(id);
    if (entry.journalRepairNeeded) {
      await fs.truncate(journalFile, entry.journalBytes);
      entry.journalRepairNeeded = false;
    }
    const changes: JournalChange[] = [];
    const deltaBytes = diff(entry.shadow, session, [], changes);
    if (!changes.length && !entry.metadataDirty) return;
    if (changes.length) {
      // The live session can change while the append waits. Apply the same values written to disk.
      const frozenChanges = copy(changes);
      const sequence = entry.sequence + 1;
      const record =
        JSON.stringify({ sequence, changes: frozenChanges }) + "\n";
      try {
        await fs.appendFile(journalFile, record, { mode: 0o600 });
      } catch (error) {
        entry.journalRepairNeeded = true;
        try {
          await fs.truncate(journalFile, entry.journalBytes);
          entry.journalRepairNeeded = false;
        } catch (repairError) {
          throw new AggregateError(
            [error, repairError],
            "Claude session journal append failed and could not be rolled back",
            { cause: error },
          );
        }
        throw error;
      }
      entry.shadow = replaySessionJournal(entry.shadow, frozenChanges);
      entry.sequence = sequence;
      entry.records++;
      entry.journalBytes += Buffer.byteLength(record);
      entry.used = ++clock;
      entry.shadowBytes += deltaBytes;
      if (entry.syncSessionBytesOnWrite) {
        entry.sessionBytes = entry.shadowBytes;
        entry.syncSessionBytesOnWrite = false;
      } else entry.sessionBytes += deltaBytes;
      retainedBytes -= entry.bytes;
      entry.bytes = Math.max(0, entry.sessionBytes + entry.shadowBytes);
      retainedBytes += entry.bytes;
      enforceLimit();
      entry.metadataDirty = true;
    }
    await writeMetadata(session);
    entry.metadataDirty = false;
    if (
      changes.length &&
      (entry.records >= compactRecords || entry.journalBytes >= compactBytes)
    ) {
      await atomicSnapshot(id, entry.shadow, entry.sequence);
      try {
        await fs.writeFile(journalFile, "", { mode: 0o600 });
      } catch (error) {
        try {
          const recovered = await readAll(id);
          if (recovered.sequence !== entry.sequence)
            throw new Error("Claude session journal changed during compaction");
          entry.records = recovered.records;
          entry.journalBytes = recovered.journalBytes;
          entry.journalRepairNeeded = false;
        } catch (repairError) {
          entry.records = 0;
          entry.journalBytes = 0;
          entry.journalRepairNeeded = true;
          throw new AggregateError(
            [error, repairError],
            "Claude session journal clear failed and its state could not be read",
            { cause: error },
          );
        }
        throw error;
      }
      entry.records = 0;
      entry.journalBytes = 0;
    }
  };

  async function load(
    id: string,
    reused: Session | null = null,
  ): Promise<Session> {
    const saved = await readAll(id);
    let shadow = copy(saved.session);
    const session = reused || saved.session;
    if (
      !reused &&
      (await reconcileNativeSession(session, options.nativeHistory))
    ) {
      await atomicSnapshot(id, session, saved.sequence);
      shadow = copy(session);
    }
    if (!reused) await reconcileRestartTurns(session, options.nativeHistory);
    const cached = sessions.get(id);
    if (cached) {
      if (cached !== session)
        throw new Error("Claude session identity conflict during load");
      return cached;
    }
    return adopt(id, session, { ...saved, session: shadow });
  }
  async function get(id: string): Promise<Session> {
    if (sessions.has(id)) {
      const entry = entries.get(id);
      if (entry) entry.used = ++clock;
      return sessions.get(id) ?? load(id);
    }
    if (!loads.has(id)) {
      const weak = evicted.get(id);
      const reused = weak?.deref() || null;
      if (weak && !reused) evicted.delete(id);
      const pending = load(id, reused);
      loads.set(id, pending);
      pending.then(
        () => {
          if (loads.get(id) === pending) loads.delete(id);
        },
        () => {
          if (loads.get(id) === pending) loads.delete(id);
        },
      );
    }
    const pending = loads.get(id);
    if (!pending) throw new Error("Claude session load was not scheduled");
    return pending;
  }
  async function metadata(id: string): Promise<SessionMetadata> {
    const live = sessions.get(id);
    if (live?.ephemeral === true) return metadataFor(live, `ephemeral:${id}`);
    const cached = metadataCache.get(id);
    if (cached) return cached;
    await ready;
    try {
      const saved = parseSessionMetadata(
        await fs.readFile(metadataFileFor(id), "utf8"),
      );
      if (saved?.id !== id || typeof saved.revision !== "string")
        throw new Error("Claude session metadata is invalid");
      metadataCache.set(id, saved);
      return saved;
    } catch (error) {
      if (!object(error) || error.code !== "ENOENT") throw error;
    }
    // Legacy sessions have no header. Load only the requested record, then
    // write a sidecar so later list and read requests avoid the transcript.
    const wasCached = sessions.has(id);
    const session = await get(id);
    const metadata = await writeMetadata(session);
    if (!wasCached) await evict(id);
    return metadata;
  }
  async function listMetadata(cursor: string | null = null, limit = 50) {
    await ready;
    const ids = (await fs.readdir(directory))
      .filter((file) => /^[a-f0-9-]{36}\.json$/.test(file))
      .map((file) => file.slice(0, -5))
      .sort();
    const offset = cursor == null || cursor === "" ? 0 : Number(cursor);
    if (!Number.isSafeInteger(offset) || offset < 0)
      throw new Error("Invalid Claude thread cursor");
    const size = Math.max(
      1,
      Math.min(100, Number.isSafeInteger(Number(limit)) ? Number(limit) : 50),
    );
    const page = ids.slice(offset, offset + size);
    const data = [];
    for (const id of page) data.push(await metadata(id));
    return {
      data,
      nextCursor: offset + size < ids.length ? String(offset + size) : null,
    };
  }
  function persist(session: Session): Promise<void> {
    // Claude title requests use Python's --no-session-persistence behavior.
    // Keep their turns in the live map for the duration of the request, but
    // never create transcript, journal, or metadata files for them.
    if (session.ephemeral === true) return Promise.resolve();
    const id = session.id;
    // A save that has not started yet diffs the live session when it starts,
    // so it already covers this caller's changes: join it instead of adding
    // another whole-session diff behind it.
    const waiting = queued.get(id);
    if (waiting?.session === session) return waiting.promise;
    const previous = writes.get(id) || Promise.resolve();
    const next = previous
      .catch(() => {})
      .then(() => {
        if (queued.get(id)?.promise === next) queued.delete(id);
        return persistNow(session);
      });
    queued.set(id, { session, promise: next });
    writes.set(id, next);
    const clear = () => {
      if (writes.get(id) === next) {
        writes.delete(id);
        enforceLimit();
      }
    };
    next.then(
      () => {
        if (unsaved.get(id) === session) {
          unsaved.delete(id);
          options.onSaved?.(session);
        }
        clear();
      },
      (error) => {
        if (isDiskFull(error)) deferSave(session);
        clear();
      },
    );
    return next;
  }
  // The disk is full: keep the live session and save it again later. The
  // journal diff starts from the last saved shadow, so nothing is lost.
  function deferSave(session: Session) {
    unsaved.set(session.id, session);
    if (unsavedTimer) return;
    unsavedTimer = setTimeout(() => {
      unsavedTimer = null;
      for (const pending of unsaved.values()) persist(pending).catch(() => {});
    }, diskFullRetryMs);
    unsavedTimer.unref?.();
  }
  // Save without failing the caller when the disk is full. Returns true when
  // the save is deferred; any other write error still rejects.
  async function persistOrDefer(session: Session) {
    try {
      await persist(session);
      return false;
    } catch (error) {
      if (!isDiskFull(error)) throw error;
      return true;
    }
  }
  async function evict(id: string) {
    const pending = writes.get(id);
    if (pending) await pending.catch(() => {});
    if (options.isPinned?.(id) || writes.has(id) || unsaved.has(id))
      return false;
    sessions.delete(id);
    const entry = entries.get(id);
    if (entry) {
      rememberEvicted(id, entry.session);
      retainedBytes -= entry.bytes;
    }
    entries.delete(id);
    return true;
  }
  async function drain() {
    await Promise.allSettled(writes.values());
    if (unsaved.size)
      await Promise.allSettled(
        [...unsaved.values()].map((session) => persist(session)),
      );
  }
  function stats() {
    return {
      cachedSessions: sessions.size,
      retainedBytes,
      pendingWrites: writes.size,
      unsavedSessions: unsaved.size,
      journalBytes: [...entries.values()].reduce(
        (sum, entry) => sum + entry.journalBytes,
        0,
      ),
    };
  }
  // This read must remain available when get() refuses a damaged session.
  // Do not call readAll(): it can compact a confirmed journal repair.
  async function repairReport(id: string) {
    await ready;
    const marker = fileFor(id).replace(/\.json$/, ".repair.json");
    let handle;
    try {
      handle = await fs.open(marker, "r");
      const buffer = Buffer.alloc(MAX_REPAIR_REPORT_BYTES + 1);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
      if (bytesRead > MAX_REPAIR_REPORT_BYTES)
        throw new Error("Claude repair report exceeds the read limit");
      const report: unknown = JSON.parse(
        buffer.subarray(0, bytesRead).toString("utf8"),
      );
      if (
        !object(report) ||
        !["needs_repair", "repaired"].includes(String(report.status)) ||
        typeof report.skippedRows !== "number" ||
        !Number.isSafeInteger(report.skippedRows) ||
        report.skippedRows < 0 ||
        typeof report.invalidSequence !== "boolean"
      )
        throw new Error("Claude repair report is invalid");
      return {
        status: report.status,
        skippedRows: report.skippedRows,
        invalidSequence: report.invalidSequence,
        scopeConfirmed:
          typeof report.scopeConfirmed === "boolean"
            ? report.scopeConfirmed
            : null,
        nativeHistoryConfirmed:
          typeof report.nativeHistoryConfirmed === "boolean"
            ? report.nativeHistoryConfirmed
            : null,
      };
    } catch (error) {
      if (!object(error) || error.code !== "ENOENT") throw error;
      return {
        status: "none",
        skippedRows: 0,
        invalidSequence: false,
        scopeConfirmed: null,
        nativeHistoryConfirmed: null,
      };
    } finally {
      await handle?.close();
    }
  }
  return {
    repairReport,
    sessions,
    get,
    metadata,
    listMetadata,
    persist,
    persistOrDefer,
    isUnsaved: (id: string) => unsaved.has(id),
    evict,
    drain,
    stats,
    readAll,
  };
}
