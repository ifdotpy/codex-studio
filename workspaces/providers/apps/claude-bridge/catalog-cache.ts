// Refresh trusted metadata only while native work remains active.
export function createCatalogCache<
  Value,
  Timer extends { unref?: () => unknown },
>({
  load,
  isActive,
  now = Date.now,
  setTimer,
  clearTimer,
  ttlMs = 300000,
  refreshMs = 240000,
}: {
  load(background: boolean): Promise<Value>;
  isActive(): boolean;
  now?: () => number;
  setTimer?: (callback: () => void, delay: number) => Timer;
  clearTimer?: (timer: Timer) => void;
  ttlMs?: number;
  refreshMs?: number;
}) {
  let confirmed: { value: Value; startedAt: number } | undefined;
  let pending: Promise<Value> | undefined;
  let timer: { unref?: () => unknown; cancel: () => void } | undefined;
  let attempted: typeof confirmed;
  let closed = false;
  const fresh = () => confirmed && now() - confirmed.startedAt < ttlMs;
  const cancelTimer = () => {
    if (timer !== undefined) timer.cancel();
    timer = undefined;
  };

  function activityChanged() {
    cancelTimer();
    if (
      closed ||
      !confirmed ||
      !fresh() ||
      pending ||
      attempted === confirmed ||
      !isActive()
    )
      return;
    const callback = () => {
      if (timer !== scheduled) return;
      timer = undefined;
      if (
        closed ||
        !fresh() ||
        pending ||
        attempted === confirmed ||
        !isActive()
      )
        return;
      attempted = confirmed;
      void refresh(true).catch(() => {});
    };
    const delay = Math.max(0, confirmed.startedAt + refreshMs - now());
    const scheduled = (() => {
      if (setTimer && clearTimer) {
        const handle = setTimer(callback, delay);
        return {
          unref: () => handle.unref?.(),
          cancel: () => clearTimer(handle),
        };
      }
      const handle = setTimeout(callback, delay);
      return {
        unref: () => handle.unref(),
        cancel: () => clearTimeout(handle),
      };
    })();
    timer = scheduled;
    timer?.unref?.();
  }

  function refresh(background = false) {
    if (pending) return pending;
    const startedAt = now();
    const result = Promise.resolve()
      .then(() => load(background))
      .then(
        (value) => {
          if (!closed) confirmed = { value, startedAt };
          return value;
        },
        (error: unknown) => {
          // A proven account rejection invalidates even an unexpired proof.
          if (
            error !== null &&
            typeof error === "object" &&
            "claudeAccountValidationFailed" in error &&
            error.claudeAccountValidationFailed === true
          )
            confirmed = undefined;
          throw error;
        },
      );
    pending = result;
    const finished = () => {
      if (pending === result) pending = undefined;
      activityChanged();
    };
    void result.then(finished, finished);
    return result;
  }

  return {
    readPassive() {
      if (closed || !confirmed)
        return Promise.reject(
          new Error("Fresh Claude account proof is unavailable"),
        );
      return fresh() ? Promise.resolve(confirmed.value) : refresh(true);
    },
    read() {
      if (closed)
        return Promise.reject(new Error("Claude metadata cache closed"));
      if (confirmed && fresh()) {
        activityChanged();
        return Promise.resolve(confirmed.value);
      }
      return refresh();
    },
    activityChanged,
    close() {
      closed = true;
      cancelTimer();
    },
  };
}

// Read metadata from a verified live query when one exists. A foreground read
// falls back to a separate probe when that refresh fails without proving an
// account change, so a busy or closing query cannot hide the model catalog.
// A background refresh never spawns a probe.
export async function loadCatalog<Live, Value>({
  background,
  live,
  refreshLive,
  probe,
}: {
  background: boolean;
  live: Live | undefined;
  refreshLive(live: Live): Promise<Value>;
  probe(): Promise<Value>;
}) {
  if (live) {
    try {
      return await refreshLive(live);
    } catch (error) {
      if (
        background ||
        (error !== null &&
          typeof error === "object" &&
          "claudeAccountValidationFailed" in error &&
          error.claudeAccountValidationFailed === true)
      )
        throw error;
    }
  }
  if (background)
    throw new Error("No live Claude query can refresh account metadata");
  return probe();
}
