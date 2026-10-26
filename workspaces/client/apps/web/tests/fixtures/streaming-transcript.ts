/** Request one growing-reply delta and wait for that exact delta to commit.
 * The caller arms its scroll witness before requesting publication. No timer
 * may publish another delta while the caller measures or settles layout. */
export function streamTranscriptFixture<T>({
  publish,
  rendered,
}: {
  publish: () => Promise<T>;
  rendered: (update: T) => Promise<void>;
}) {
  return async () => {
    const update = await publish();
    await rendered(update);
  };
}
