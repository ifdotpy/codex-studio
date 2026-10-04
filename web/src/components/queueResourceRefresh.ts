export function createQueueResourceRefresh(
  isLocked: () => boolean,
  refresh: () => Promise<void>,
) {
  let pending = false;

  return {
    async invalidate() {
      if (isLocked()) {
        pending = true;
        return;
      }
      await refresh();
    },
    async flush() {
      if (!pending || isLocked()) return;
      pending = false;
      await refresh();
    },
  };
}
