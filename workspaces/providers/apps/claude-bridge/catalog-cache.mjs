// Refresh trusted metadata only while native work remains active.
export function createCatalogCache({
  load,
  isActive,
  now = Date.now,
  setTimer = setTimeout,
  clearTimer = clearTimeout,
  ttlMs = 300000,
  refreshMs = 240000,
}) {
  let confirmed, pending, timer, attempted;
  let closed = false;
  const fresh = () => confirmed && now() - confirmed.startedAt < ttlMs;
  const cancelTimer = () => {
    if (timer !== undefined) clearTimer(timer);
    timer = undefined;
  };

  function activityChanged() {
    cancelTimer();
    if (closed || !fresh() || pending || attempted === confirmed || !isActive())
      return;
    const scheduled = setTimer(
      () => {
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
      },
      Math.max(0, confirmed.startedAt + refreshMs - now()),
    );
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
        (error) => {
          // A proven account rejection invalidates even an unexpired proof.
          if (error?.claudeAccountValidationFailed === true)
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
      if (fresh()) {
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
export async function loadCatalog({ background, live, refreshLive, probe }) {
  if (live) {
    try {
      return await refreshLive(live);
    } catch (error) {
      if (background || error?.claudeAccountValidationFailed === true)
        throw error;
    }
  }
  if (background)
    throw new Error("No live Claude query can refresh account metadata");
  return probe();
}
