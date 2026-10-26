const initialize = async () => {
  const send = await import("../../src/sync/send.ts");
  window.retrySendSend = send.durableSend;
  const r = await import("react"),
    d = await import("react-dom/client");
  const React = r.default || r,
    { createRoot } = d.default || d;
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.outbox = send.useOutbox();
      return null;
    }),
  );
};
window.retrySendInitialize = initialize;

declare global {
  interface Window {
    retrySendInitialize: typeof initialize;
    retrySendSend: typeof import("../../src/sync/send.ts").durableSend;
    sendResult: ReturnType<typeof import("../../src/sync/send.ts").durableSend>;
  }
}
export {};
