export function foregroundTranscriptPending(
  foregroundId: string | null,
  foregroundReady: boolean,
) {
  return foregroundId !== null && !foregroundReady;
}
