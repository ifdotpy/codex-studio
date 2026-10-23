// Interval between scroll fixture transcript updates, milliseconds.
const STREAM_INTERVAL_MS = 50;

/** Publish the scroll fixture's growing reply until its caller stops it. */
export async function streamTranscriptFixture({ running, publish, rendered }) {
  while (running()) {
    const update = await publish();
    // Persisting and rendering are asynchronous. A fixed publication interval
    // alone can overflow the renderer's bounded delivery queue on a slow host.
    await rendered(update);
    await new Promise((resolve) => setTimeout(resolve, STREAM_INTERVAL_MS));
  }
}
