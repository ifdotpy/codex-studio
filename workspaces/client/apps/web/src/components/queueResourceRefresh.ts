export function createQueueResourceRefresh(
  isLocked: () => boolean,
  refresh: () => Promise<void>,
) {
  let pending = false;

  return {
    get pending() {
      return pending;
    },
    async invalidate() {
      if (isLocked()) {
        pending = true;
        return;
      }
      pending = false;
      await refresh();
    },
    async flush() {
      if (!pending || isLocked()) return;
      pending = false;
      await refresh();
    },
  };
}
