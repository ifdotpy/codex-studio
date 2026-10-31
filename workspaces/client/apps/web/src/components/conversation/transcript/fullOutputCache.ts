/**
 * Full text of a cut item, cached by content hash. The hash names the exact
 * text, so one fetch serves every later open; a failed fetch is not kept.
 */
const texts = new Map<string, Promise<string>>();

export function fullOutputText(
  hash: string,
  fetchText: (hash: string) => Promise<string>,
): Promise<string> {
  const cached = texts.get(hash);
  if (cached) return cached;
  const pending = fetchText(hash);
  texts.set(hash, pending);
  pending.catch(() => {
    if (texts.get(hash) === pending) texts.delete(hash);
  });
  return pending;
}

export function clearFullOutputCache(): void {
  texts.clear();
}
