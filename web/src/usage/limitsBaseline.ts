export function limitsBaselineReader(
  isHydrated: () => boolean,
  read: () => Promise<void>,
): () => Promise<void> {
  let baseline = true;
  return async () => {
    if (baseline) {
      baseline = false;
      if (isHydrated()) return;
    }
    await read();
  };
}
