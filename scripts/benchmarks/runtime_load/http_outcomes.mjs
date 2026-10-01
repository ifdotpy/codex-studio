export function readRetryKey(tab, url) {
  const parsed = new URL(url);
  if (parsed.pathname !== "/api/sync/pull") return null;
  return [
    tab,
    parsed.pathname,
    parsed.searchParams.get("scope") || "state",
    parsed.searchParams.get("after") || "0",
    parsed.searchParams.get("limit") || "100",
  ].join("\u0000");
}

export function isRetryableSnapshotDeferred(status, method, url, detail) {
  if (status !== 503 || method !== "GET") return false;
  if (new URL(url).pathname !== "/api/sync/pull") return false;
  let body;
  try {
    body = typeof detail === "string" ? JSON.parse(detail) : detail;
  } catch {
    return false;
  }
  return (
    body?.error ===
    "Runtime snapshot is temporarily unavailable; retry shortly."
  );
}

export function isExpectedSyntheticUnavailable(status, url, detail) {
  if (status !== 400) return false;
  const parsed = new URL(url);
  if (!["/api/models", "/api/costs", "/api/limits"].includes(parsed.pathname))
    return false;
  if (!parsed.searchParams.get("account_key")?.startsWith("bench-"))
    return false;
  let body;
  try {
    body = typeof detail === "string" ? JSON.parse(detail) : detail;
  } catch {
    return false;
  }
  return (
    typeof body?.error === "string" &&
    /unknown codex account|account unavailable|account not found/i.test(
      body.error,
    )
  );
}

export class HttpOutcomeTracker {
  #pendingReads = new Map();
  retryableSnapshotDeferred = [];
  expectedUnavailable = [];
  failures = [];

  record({ tab, status, url, method, detail, atEpochMs }) {
    if (status < 400) {
      if (status === 200 && method === "GET") {
        const key = readRetryKey(tab, url);
        const pending = key && this.#pendingReads.get(key);
        if (pending?.length) {
          for (const prior of pending) {
            prior.recoveredAtEpochMs = atEpochMs;
            prior.recoveryLatencyMs = atEpochMs - prior.atEpochMs;
            prior.recoveredByStatus = status;
          }
          this.#pendingReads.delete(key);
        }
      }
      return;
    }

    if (isRetryableSnapshotDeferred(status, method, url, detail)) {
      const key = readRetryKey(tab, url);
      const attempt = {
        tab,
        url,
        method,
        scope: new URL(url).searchParams.get("scope") || "state",
        atEpochMs,
        recoveredAtEpochMs: null,
        recoveryLatencyMs: null,
        recoveredByStatus: null,
      };
      const pending = this.#pendingReads.get(key) || [];
      pending.push(attempt);
      this.#pendingReads.set(key, pending);
      this.retryableSnapshotDeferred.push(attempt);
      return;
    }

    if (isExpectedSyntheticUnavailable(status, url, detail)) {
      this.expectedUnavailable.push({ tab, status, url, detail });
      return;
    }
    this.failures.push({ tab, status, url, method, detail });
  }

  unrecoveredSnapshotReads() {
    return this.retryableSnapshotDeferred.filter(
      (attempt) => attempt.recoveredAtEpochMs === null,
    );
  }
}
