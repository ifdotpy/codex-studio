import { createHash } from "node:crypto";
import { idleRefusal, promptPrefix } from "./idle-compaction.mjs";

// SDK forkSession remaps UUIDs and renews the final assistant timestamp (LF
// in SDK 0.3.308). Keep all content and delivery flags in the comparison.
// The separate source fingerprint includes the original UUIDs and timestamps.
function contents(messages) {
  const lastAssistant = messages.findLastIndex(
    (message) => message.type === "assistant",
  );
  return JSON.stringify(
    messages.map(
      ({ uuid: _uuid, session_id: _sessionId, ...content }, index) => {
        if (index === lastAssistant) delete content.timestamp;
        return content;
      },
    ),
  );
}
function fingerprint(messages) {
  return createHash("sha256").update(JSON.stringify(messages)).digest("hex");
}
async function history(s, id, deps) {
  const messages = await deps.getSessionMessages(id, {
    dir: s.nativeHistoryCwd || s.cwd,
    includeSystemMessages: true,
  });
  if (!messages.length)
    throw new Error("Claude summary history is unavailable");
  return messages;
}

export async function prepareIdleSummary(
  s,
  requestId,
  expected,
  effective,
  deps,
) {
  if (s.idleSummary?.requestId === requestId)
    throw new Error(
      "Claude summary preparation already has a receipt; do not repeat it",
    );
  const refusal = idleRefusal(s, null, expected, effective);
  if (refusal || !deps.isCurrent())
    throw new Error("Claude summary source changed");
  const sourceNativeId = s.nativeId || s.id;
  const messages = await history(s, sourceNativeId, deps);
  if (!deps.isCurrent() || idleRefusal(s, null, expected, effective))
    throw new Error("Claude summary source changed before preparation");
  const receipt = {
    requestId,
    status: "preparing",
    sourceNativeId,
    sourceFingerprint: fingerprint(messages),
    prefix: promptPrefix(s, effective),
    cacheRequestId: expected.requestId,
  };
  s.idleSummary = receipt;
  // This uncertainty receipt precedes every native mutation.
  await deps.persist(s);
  try {
    const fork = await deps.forkSession(sourceNativeId, {
      dir: s.nativeHistoryCwd || s.cwd,
    });
    if (!fork?.sessionId || fork.sessionId === sourceNativeId)
      throw new Error("Claude returned an invalid summary fork");
    receipt.targetNativeId = fork.sessionId;
    const copied = await history(s, fork.sessionId, deps);
    if (contents(copied) !== contents(messages))
      throw new Error("Claude summary fork history differs");
    if (!deps.isCurrent() || idleRefusal(s, null, expected, effective))
      throw new Error("Claude summary source changed before the summary query");
    const metadata = await deps.compact(fork.sessionId);
    if (
      !metadata ||
      !deps.isCurrent() ||
      (s.nativeId || s.id) !== sourceNativeId ||
      promptPrefix(s, effective) !== receipt.prefix ||
      fingerprint(await history(s, sourceNativeId, deps)) !==
        receipt.sourceFingerprint
    )
      throw new Error("Claude summary source changed during preparation");
    receipt.metadata = metadata;
    receipt.targetFingerprint = fingerprint(
      await history(s, fork.sessionId, deps),
    );
    receipt.status = "ready";
    await deps.persist(s);
    return receipt;
  } catch (error) {
    receipt.status = "failed";
    await deps.persist(s).catch(() => {});
    throw error;
  }
}

export async function applyIdleSummary(s, effective, deps) {
  const receipt = s.idleSummary;
  if (receipt?.status !== "ready") return null;
  const current =
    (s.nativeId || s.id) === receipt.sourceNativeId &&
    promptPrefix(s, effective) === receipt.prefix &&
    deps.isCurrent();
  let valid = false;
  try {
    valid =
      current &&
      fingerprint(await history(s, receipt.sourceNativeId, deps)) ===
        receipt.sourceFingerprint &&
      fingerprint(await history(s, receipt.targetNativeId, deps)) ===
        receipt.targetFingerprint &&
      deps.isCurrent() &&
      (s.nativeId || s.id) === receipt.sourceNativeId &&
      promptPrefix(s, effective) === receipt.prefix;
  } catch {
    // A missing or unreadable optional summary must keep the original history.
  }
  if (!valid) {
    receipt.status = "discarded";
    await deps.persist(s);
    return null;
  }
  const before = structuredClone(s);
  s.historyBranches ||= [];
  s.historyBranches.push({
    requestId: receipt.requestId,
    nativeId: receipt.sourceNativeId,
    turns: structuredClone(s.turns),
  });
  s.nativeId = receipt.targetNativeId;
  receipt.status = "applied";
  receipt.noticePending = true;
  try {
    await deps.persist(s);
  } catch (error) {
    for (const key of Object.keys(s)) if (!(key in before)) delete s[key];
    Object.assign(s, before);
    throw error;
  }
  return receipt;
}
