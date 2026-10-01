import fs from "node:fs/promises";
import path from "node:path";
import { randomUUID } from "node:crypto";

const FORMAT = "studio-claude-session-v1";
const DEFAULT_CACHE_BYTES = 64 * 1024 * 1024;
const DEFAULT_COMPACT_BYTES = 1024 * 1024;
const DEFAULT_COMPACT_RECORDS = 128;

function copy(value) {
  return structuredClone(value);
}

function estimateJsonBytes(value) {
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
  const entries = Object.entries(value).filter(
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

function diff(before, after, location = [], changes = []) {
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
    !Array.isArray(before) &&
    !Array.isArray(after)
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

function apply(session, changes) {
  for (const change of changes) {
    let parent = session;
    for (const key of change.path.slice(0, -1)) parent = parent[key];
    const key = change.path.at(-1);
    if (change.op === "append") parent[key].push(...change.value);
    else if (change.op === "delete") delete parent[key];
    else if (change.path.length) parent[key] = change.value;
    else session = change.value;
  }
  return session;
}

export function createSessionStore(root, options = {}) {
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
  const sessions = new Map();
  const entries = new Map();
  const writes = new Map();
  const loads = new Map();
  const evicted = new Map();
  const metadataCache = new Map();
  const finalizer = new FinalizationRegistry(({ id, ref }) => {
    if (evicted.get(id) === ref) evicted.delete(id);
  });
  let clock = 0;
  let retainedBytes = 0;

  const fileFor = (id) => {
    if (!/^[a-f0-9-]{36}$/.test(id))
      throw new Error("Invalid Claude session identity");
    return path.join(directory, id + ".json");
  };
  const logFor = (id) => fileFor(id).replace(/\.json$/, ".jsonl");
  const metadataFileFor = (id) => fileFor(id).replace(/\.json$/, ".meta.json");
  const metadataFor = (session, revision) => ({
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
  const writeMetadata = async (session, revision = randomUUID()) => {
    const metadata = metadataFor(session, revision);
    const file = metadataFileFor(session.id),
      tmp = file + "." + randomUUID() + ".tmp";
    await fs.writeFile(tmp, JSON.stringify(metadata), { mode: 0o600 });
    await fs.rename(tmp, file);
    metadataCache.set(session.id, metadata);
    return metadata;
  };
  const readBase = async (id) => {
    await ready;
    const raw = await fs.readFile(fileFor(id), "utf8");
    const saved = JSON.parse(raw);
    if (saved?.format === FORMAT)
      return { session: saved.session, sequence: saved.sequence };
    return { session: saved, sequence: 0 };
  };
  const readAll = async (id) => {
    const base = await readBase(id);
    let log = "";
    try {
      log = await fs.readFile(logFor(id), "utf8");
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    const completeLines = log.split("\n");
    const tornTail = completeLines.at(-1) !== "";
    if (tornTail) completeLines.pop();
    const validLogBytes = tornTail
      ? log.lastIndexOf("\n") + 1
      : Buffer.byteLength(log);
    if (tornTail) await fs.truncate(logFor(id), validLogBytes);
    let records = 0;
    for (const line of completeLines) {
      if (!line) continue;
      const record = JSON.parse(line);
      if (record.sequence <= base.sequence) continue;
      if (record.sequence !== base.sequence + 1)
        throw new Error("Claude session journal sequence is invalid");
      base.session = apply(base.session, record.changes);
      base.sequence = record.sequence;
      records++;
    }
    base.records = records;
    base.journalBytes = validLogBytes;
    return base;
  };
  // Measure both retained objects only when the cache adopts or creates an entry.
  const measure = (entry) => {
    entry.sessionBytes = estimateJsonBytes(entry.session);
    entry.shadowBytes = estimateJsonBytes(entry.shadow);
    entry.bytes = entry.sessionBytes + entry.shadowBytes;
  };
  const rememberEvicted = (id, session) => {
    const ref = new WeakRef(session);
    evicted.set(id, ref);
    finalizer.unregister(session);
    finalizer.register(session, { id, ref }, session);
  };
  const adopt = (id, session, saved) => {
    const entry = {
      session,
      shadow: saved.session,
      sequence: saved.sequence,
      records: saved.records || 0,
      journalBytes: saved.journalBytes || 0,
      used: ++clock,
      syncSessionBytesOnWrite: true,
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
      let candidate;
      for (const [id, entry] of entries) {
        if (options.isPinned?.(id) || writes.has(id)) continue;
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
    session,
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
    const entry = {
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
  const atomicSnapshot = async (id, session, sequence) => {
    const file = fileFor(id),
      tmp = file + "." + randomUUID() + ".tmp";
    await fs.writeFile(
      tmp,
      JSON.stringify({ format: FORMAT, sequence, session }),
      { mode: 0o600 },
    );
    await fs.rename(tmp, file);
  };
  const persistNow = async (session) => {
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
        if (error.code !== "ENOENT") throw error;
        const initial = copy(session);
        await atomicSnapshot(id, initial, 0);
        await fs.writeFile(logFor(id), "", { mode: 0o600 });
        await writeMetadata(session);
        remember(session, 0, 0, 0, initial, true);
        return;
      }
      entry = {
        session,
        shadow: saved.session,
        sequence: saved.sequence,
        records: saved.records || 0,
        journalBytes: saved.journalBytes || 0,
        used: ++clock,
        syncSessionBytesOnWrite: true,
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
    const changes = [];
    const deltaBytes = diff(entry.shadow, session, [], changes);
    if (!changes.length) return;
    const sequence = entry.sequence + 1;
    const record = JSON.stringify({ sequence, changes }) + "\n";
    await fs.appendFile(logFor(id), record, { mode: 0o600 });
    entry.shadow = apply(entry.shadow, copy(changes));
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
    await writeMetadata(session);
    if (entry.records >= compactRecords || entry.journalBytes >= compactBytes) {
      await atomicSnapshot(id, entry.shadow, entry.sequence);
      await fs.writeFile(logFor(id), "", { mode: 0o600 });
      entry.records = 0;
      entry.journalBytes = 0;
    }
  };

  async function load(id, reused = null) {
    const saved = await readAll(id);
    const shadow = copy(saved.session);
    const session = reused || saved.session;
    if (!reused) {
      for (const turn of session.turns)
        if (turn.status === "inProgress") {
          turn.status = "interrupted";
          turn.error = {
            message:
              "Claude connection ended. Review the saved transcript before continuing.",
          };
        }
    }
    const cached = sessions.get(id);
    if (cached) {
      if (cached !== session)
        throw new Error("Claude session identity conflict during load");
      return cached;
    }
    return adopt(id, session, { ...saved, session: shadow });
  }
  async function get(id) {
    if (sessions.has(id)) {
      const entry = entries.get(id);
      if (entry) entry.used = ++clock;
      return sessions.get(id);
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
    return loads.get(id);
  }
  async function metadata(id) {
    const cached = metadataCache.get(id);
    if (cached) return cached;
    await ready;
    try {
      const saved = JSON.parse(await fs.readFile(metadataFileFor(id), "utf8"));
      if (saved?.id !== id || typeof saved.revision !== "string")
        throw new Error("Claude session metadata is invalid");
      metadataCache.set(id, saved);
      return saved;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    // Legacy sessions have no header. Load only the requested record, then
    // write a sidecar so later list and read requests avoid the transcript.
    const wasCached = sessions.has(id);
    const session = await get(id);
    const metadata = await writeMetadata(session);
    if (!wasCached) await evict(id);
    return metadata;
  }
  async function listMetadata(cursor = null, limit = 50) {
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
  function persist(session) {
    const id = session.id;
    const previous = writes.get(id) || Promise.resolve();
    const next = previous.catch(() => {}).then(() => persistNow(session));
    writes.set(id, next);
    const clear = () => {
      if (writes.get(id) === next) {
        writes.delete(id);
        enforceLimit();
      }
    };
    next.then(clear, clear);
    return next;
  }
  async function evict(id) {
    const pending = writes.get(id);
    if (pending) await pending.catch(() => {});
    if (options.isPinned?.(id) || writes.has(id)) return false;
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
  }
  function stats() {
    return {
      cachedSessions: sessions.size,
      retainedBytes,
      pendingWrites: writes.size,
      journalBytes: [...entries.values()].reduce(
        (sum, entry) => sum + entry.journalBytes,
        0,
      ),
    };
  }
  return {
    sessions,
    get,
    metadata,
    listMetadata,
    persist,
    evict,
    drain,
    stats,
    readAll,
  };
}
