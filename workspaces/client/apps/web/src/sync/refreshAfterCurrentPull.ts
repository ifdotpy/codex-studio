export async function refreshAfterCurrentPull<T>(
  currentPull: Promise<unknown> | undefined,
  refresh: () => Promise<T>,
): Promise<T> {
  if (currentPull) await currentPull.catch(() => {});
  return refresh();
}
