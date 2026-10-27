// End the probe's observation only after it has consumed the fixture's event.
const deadline = new AbortController();
const timeout = AbortSignal.timeout.bind(AbortSignal);
AbortSignal.timeout = (milliseconds) =>
  AbortSignal.any([deadline.signal, timeout(milliseconds)]);

const fetchWithDeadline = globalThis.fetch;
globalThis.fetch = async (...args) => {
  const response = await fetchWithDeadline(...args);
  if (!response.headers.get("content-type")?.includes("text/event-stream"))
    return response;
  const body = response.body;
  if (!body) throw new Error("Missing fixture stream");
  const reason = process.env.APPROVAL_DEADLINE_EVENT;
  Object.defineProperty(response, "body", {
    value: {
      async *[Symbol.asyncIterator]() {
        let buffer = "";
        const decoder = new TextDecoder();
        for await (const chunk of body) {
          buffer += decoder.decode(chunk, { stream: true });
          yield chunk;
          let end: number;
          while ((end = buffer.indexOf("\n\n")) >= 0) {
            const frame = buffer.slice(0, end);
            buffer = buffer.slice(end + 2);
            if (frame.includes(`"reason":"${reason}"`))
              deadline.abort(
                new DOMException("Fixture deadline", "TimeoutError"),
              );
          }
        }
      },
    },
  });
  return response;
};
