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
        void refresh().catch(() => {});
      },
      Math.max(0, confirmed.startedAt + refreshMs - now()),
    );
    timer = scheduled;
    timer?.unref?.();
  }

  function refresh() {
    if (pending) return pending;
    const startedAt = now();
    const result = Promise.resolve()
      .then(load)
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
