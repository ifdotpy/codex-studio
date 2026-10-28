import { BottomInsetBand } from "./hooks/BottomInsetBand";
import "./browser-reset.css";
import { NativeServerCredentials } from "./native/serverCredentials";
import {
  bootstrapPreferenceCache,
  applyCachedPreferenceAppearance,
} from "./sync/uiPreferenceStore";
import {
  DesktopServerCredentials,
  nativeCredentialBridge,
} from "./servers/desktopCredentials";
import { setServerCredentialAdapter } from "./servers/transport";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import App from "./App";
import { SidebarWarningsProvider } from "./components/SidebarWarnings";
import MultiServerApp from "./servers/MultiServerApp";
import { isServerView } from "./servers/environment";
import { isClassicServerView } from "./servers/environment";
import "./servers/servers.css";
import UIErrorBoundary from "./components/UIErrorBoundary";
import { theme } from "./theme";
import "./style.css";
import "./workspace-layout.css";
import "./appearance.css";
import "./studio-preferences.css";
import "./visual-activity.css";
import "./design/state.css";
import { initializeNativeClose } from "./native/close";

async function mount() {
  await initializeNativeClose();
  bootstrapPreferenceCache();
  applyCachedPreferenceAppearance();
  const nativeCredentials = nativeCredentialBridge();
  setServerCredentialAdapter(
    // The keys stay in a client core (the desktop or the server of a browser
    // page). Each paired server's requests go through its own core.
    nativeCredentials
      ? new DesktopServerCredentials(nativeCredentials)
      : new NativeServerCredentials(),
  );
  if (isServerView) document.documentElement.dataset.serverView = "true";
  if (isClassicServerView)
    document.documentElement.dataset.serverNavigation = "classic";

  const container = document.getElementById("root")!;
  if (container.dataset.studioMounted !== "true") {
    container.dataset.studioMounted = "true";
    createRoot(container).render(
      <UIErrorBoundary label="Studio" fullPage>
        <MantineProvider theme={theme} defaultColorScheme="auto">
          <BottomInsetBand>
            <SidebarWarningsProvider>
              {isServerView ? <App /> : <MultiServerApp />}
            </SidebarWarningsProvider>
          </BottomInsetBand>
        </MantineProvider>
      </UIErrorBoundary>,
    );
  }
}
void mount().catch((error) => {
  const container = document.getElementById("root");
  if (container)
    container.textContent = `Studio could not start: ${String(error)}`;
});
