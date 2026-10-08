export function limitsBaselineReader(
  hydrate: () => Promise<boolean>,
  read: () => Promise<void>,
  isActive: () => boolean = () => true,
): () => Promise<void> {
  let baseline = true;
  return async () => {
    const isBaseline = baseline;
    baseline = false;
    if (isBaseline) {
      if (await hydrate().catch(() => false)) return;
      if (!isActive()) return;
    }
    await read();
  };
}
