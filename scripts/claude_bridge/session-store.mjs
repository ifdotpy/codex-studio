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
  if (typeof value === "string") return Buffer.byteLength(JSON.stringify(value));
  if (typeof value === "number") return Buffer.byteLength(String(value));
  if (typeof value === "boolean") return value ? 4 : 5;
  if (Array.isArray(value))
    return 2 + value.reduce((sum, item) => sum + estimateJsonBytes(item === undefined ? null : item), 0) + Math.max(0, value.length - 1);
  const entries = Object.entries(value).filter(([, item]) => item !== undefined);
  return 2 + entries.reduce((sum, [key, item]) =>
    sum + Buffer.byteLength(JSON.stringify(key)) + 1 + estimateJsonBytes(item), 0) +
    Math.max(0, entries.length - 1);
}

function diff(before, after, location = [], changes = []) {
  if (before === after) return changes;
  if (before == null || after == null || typeof before !== "object" || typeof after !== "object") {
    changes.push({ op: "set", path: location, value: after });
    return changes;
  }
  if (Array.isArray(before) && Array.isArray(after)) {
    const shared = Math.min(before.length, after.length);
    for (let i = 0; i < shared; i++) diff(before[i], after[i], [...location, i], changes);
    if (after.length > before.length)
      changes.push({ op: "append", path: location, value: after.slice(before.length) });
    else if (after.length < before.length)
      changes.push({ op: "set", path: location, value: after });
    return changes;
  }
  if (before && after && typeof before === "object" && typeof after === "object" &&
      !Array.isArray(before) && !Array.isArray(after)) {
    for (const key of Object.keys(before))
      if (!(key in after)) changes.push({ op: "delete", path: [...location, key] });
    for (const [key, value] of Object.entries(after)) {
      if (!(key in before)) changes.push({ op: "set", path: [...location, key], value });
      else diff(before[key], value, [...location, key], changes);
    }
    return changes;
  }
  changes.push({ op: "set", path: location, value: after });
  return changes;
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
  const maxCacheBytes = options.maxCacheBytes ??
    (Number.isFinite(configuredCacheBytes) && configuredCacheBytes > 0
      ? configuredCacheBytes
      : DEFAULT_CACHE_BYTES);
  const compactBytes = options.compactBytes ?? DEFAULT_COMPACT_BYTES;
  const compactRecords = options.compactRecords ?? DEFAULT_COMPACT_RECORDS;
  const sessions = new Map();
  const entries = new Map();
  const writes = new Map();
  const loads = new Map();
  let clock = 0;
  let retainedBytes = 0;

  const fileFor = (id) => {
    if (!/^[a-f0-9-]{36}$/.test(id)) throw new Error("Invalid Claude session identity");
    return path.join(directory, id + ".json");
  };
  const logFor = (id) => fileFor(id).replace(/\.json$/, ".jsonl");
  const readBase = async (id) => {
    await ready;
    const raw = await fs.readFile(fileFor(id), "utf8");
    const saved = JSON.parse(raw);
    if (saved?.format === FORMAT) return { session: saved.session, sequence: saved.sequence };
    return { session: saved, sequence: 0 };
  };
  const readAll = async (id) => {
    const base = await readBase(id);
    let log = "";
    try { log = await fs.readFile(logFor(id), "utf8"); }
    catch (error) { if (error.code !== "ENOENT") throw error; }
    const completeLines = log.split("\n");
    const tornTail = completeLines.at(-1) !== "";
    if (tornTail) completeLines.pop();
    const validLogBytes = tornTail ? log.lastIndexOf("\n") + 1 : Buffer.byteLength(log);
    if (tornTail) await fs.truncate(logFor(id), validLogBytes);
    let records = 0;
    for (const line of completeLines) {
      if (!line) continue;
      const record = JSON.parse(line);
      if (record.sequence <= base.sequence) continue;
      if (record.sequence !== base.sequence + 1) throw new Error("Claude session journal sequence is invalid");
      base.session = apply(base.session, record.changes);
      base.sequence = record.sequence;
      records++;
    }
    base.records = records;
    base.journalBytes = validLogBytes;
    return base;
  };
  // Count the session plus its persisted shadow without allocating a full JSON string.
  const measure = (entry) => estimateJsonBytes(entry.session) + estimateJsonBytes(entry.shadow);
  const enforceLimit = () => {
    while (retainedBytes > maxCacheBytes) {
      let candidate;
      for (const [id, entry] of entries) {
        if (options.isPinned?.(id) || writes.has(id)) continue;
        if (!candidate || entry.used < candidate[1].used) candidate = [id, entry];
      }
      if (!candidate) break;
      const [id, entry] = candidate;
      sessions.delete(id);
      entries.delete(id);
      retainedBytes -= entry.bytes;
    }
  };
  const remember = (session, sequence = 0, records = 0, journalBytes = 0) => {
    const id = session.id;
    sessions.set(id, session);
    const previous = entries.get(id);
    if (previous) retainedBytes -= previous.bytes;
    const entry = {
      session,
      shadow: previous?.shadow || copy(session),
      sequence: previous?.sequence ?? sequence,
      records: previous?.records ?? records,
      journalBytes: previous?.journalBytes ?? journalBytes,
      used: ++clock,
      bytes: 0,
    };
    entry.bytes = measure(entry);
    entries.set(id, entry);
    retainedBytes += entry.bytes;
    enforceLimit();
    return session;
  };
  const atomicSnapshot = async (id, session, sequence) => {
    const file = fileFor(id), tmp = file + "." + randomUUID() + ".tmp";
    await fs.writeFile(tmp, JSON.stringify({ format: FORMAT, sequence, session }), { mode: 0o600 });
    await fs.rename(tmp, file);
  };
  const persistNow = async (session) => {
    await ready;
    const id = session.id;
    const entry = entries.get(id);
    if (!entry) {
      let sequence = 0;
      try { sequence = (await readAll(id)).sequence; }
      catch (error) { if (error.code !== "ENOENT") throw error; }
      await atomicSnapshot(id, session, sequence);
      await fs.writeFile(logFor(id), "", { mode: 0o600 });
      remember(session, sequence, 0, 0);
      return;
    }
    const changes = diff(entry.shadow, session);
    if (!changes.length) return;
    const sequence = entry.sequence + 1;
    const record = JSON.stringify({ sequence, changes }) + "\n";
    await fs.appendFile(logFor(id), record, { mode: 0o600 });
    entry.shadow = copy(session);
    entry.sequence = sequence;
    entry.records++;
    entry.journalBytes += Buffer.byteLength(record);
    entry.used = ++clock;
    retainedBytes -= entry.bytes;
    entry.bytes = measure(entry);
    retainedBytes += entry.bytes;
    enforceLimit();
    if (entry.records >= compactRecords || entry.journalBytes >= compactBytes) {
      await atomicSnapshot(id, session, entry.sequence);
      await fs.writeFile(logFor(id), "", { mode: 0o600 });
      entry.records = 0;
      entry.journalBytes = 0;
    }
  };

  async function load(id) {
    const saved = await readAll(id);
    for (const turn of saved.session.turns)
      if (turn.status === "inProgress") {
        turn.status = "interrupted";
        turn.error = { message: "Claude connection ended. Review the saved transcript before continuing." };
      }
    const entry = {
      session: saved.session,
      shadow: copy(saved.session),
      sequence: saved.sequence,
      records: saved.records,
      journalBytes: saved.journalBytes,
      used: ++clock,
      bytes: 0,
    };
    entry.bytes = measure(entry);
    sessions.set(id, saved.session);
    entries.set(id, entry);
    retainedBytes += entry.bytes;
    enforceLimit();
    return saved.session;
  }
  async function get(id) {
    if (sessions.has(id)) {
      const entry = entries.get(id);
      if (entry) entry.used = ++clock;
      return sessions.get(id);
    }
    if (!loads.has(id)) {
      const pending = load(id);
      loads.set(id, pending);
      pending.then(
        () => { if (loads.get(id) === pending) loads.delete(id); },
        () => { if (loads.get(id) === pending) loads.delete(id); },
      );
    }
    return loads.get(id);
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
    if (entry) retainedBytes -= entry.bytes;
    entries.delete(id);
    return true;
  }
  async function drain() {
    await Promise.allSettled([...writes.values()]);
  }
  function stats() {
    return {
      cachedSessions: sessions.size,
      retainedBytes,
      pendingWrites: writes.size,
      journalBytes: [...entries.values()].reduce((sum, entry) => sum + entry.journalBytes, 0),
    };
  }
  return { sessions, get, persist, evict, drain, stats, readAll };
}
