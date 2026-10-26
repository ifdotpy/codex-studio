/** Request one growing-reply delta and wait for that exact delta to commit.
 * The caller arms its scroll witness before requesting publication. No timer
 * may publish another delta while the caller measures or settles layout. */
export function streamTranscriptFixture({ publish, rendered }) {
  return async () => {
    const update = await publish();
    await rendered(update);
  };
}
