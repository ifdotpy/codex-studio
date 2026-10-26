import { fixtureElement } from "./browser-helpers.ts";
const mount1 = async () => {
  window.entityFirstLoadRetryBrowserFixtureSnapshotMountedAt =
    performance.now();
  const { useSnapshot } = await import("../../src/hooks.ts");
  const React = await import("react");
  const { createRoot } = await import("react-dom/client");
  const root = createRoot(fixtureElement("root"));
  window.entityFirstLoadRetryBrowserFixtureUnmountSnapshot = () =>
    root.unmount();
  root.render(
    React.createElement(function SnapshotHarness() {
      window.entityFirstLoadRetryBrowserFixtureSnapshot = useSnapshot();
      return React.createElement(
        "p",
        {
          role: window.entityFirstLoadRetryBrowserFixtureSnapshot.error
            ? "alert"
            : "status",
        },
        window.entityFirstLoadRetryBrowserFixtureSnapshot.error ||
          "Connecting to LAYR Studio…",
      );
    }),
  );
};
window.entityFirstLoadRetryBrowserMount1 = mount1;
declare global {
  interface Window {
    entityFirstLoadRetryBrowserMount1: typeof mount1;
  }
}

declare global {
  interface Window {
    entityFirstLoadRetryBrowserFixtureSnapshotMountedAt: number;
    entityFirstLoadRetryBrowserFixtureUnmountSnapshot: () => void;
    entityFirstLoadRetryBrowserFixtureSnapshot: ReturnType<
      typeof import("../../src/hooks.ts").useSnapshot
    >;
  }
}
