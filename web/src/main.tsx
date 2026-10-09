import {
  bootstrapPreferenceCache,
  applyCachedPreferenceAppearance,
} from "./sync/uiPreferenceStore";
import {
  DesktopServerCredentials,
  nativeCredentialBridge,
} from "./servers/desktopCredentials";
import { BrowserServerCredentials } from "./servers/browserCredentials";
import { setServerCredentialAdapter } from "./servers/transport";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import App from "./App";
import MultiServerApp from "./servers/MultiServerApp";
import { isServerView } from "./servers/environment";
import { isRemoteServerView } from "./servers/environment";
import { shellCredentialBridge } from "./servers/shellTransport";
import "./servers/servers.css";
import UIErrorBoundary from "./components/UIErrorBoundary";
import { theme } from "./theme";
import "./style.css";
import "./workspace-layout.css";
import "./appearance.css";
import "./studio-preferences.css";
import "./visual-activity.css";

bootstrapPreferenceCache();
applyCachedPreferenceAppearance();
const nativeCredentials = nativeCredentialBridge();
setServerCredentialAdapter(
  nativeCredentials
    ? new DesktopServerCredentials(nativeCredentials)
    : isRemoteServerView
      ? new BrowserServerCredentials()
      : isServerView && window.parent !== window
        ? new DesktopServerCredentials(shellCredentialBridge())
        : new BrowserServerCredentials(),
);
if (isServerView) document.documentElement.dataset.serverView = "true";

const container = document.getElementById("root")!;
if (container.dataset.studioMounted !== "true") {
  container.dataset.studioMounted = "true";
  createRoot(container).render(
    <UIErrorBoundary label="Studio" fullPage>
      <MantineProvider theme={theme} defaultColorScheme="auto">
        {isServerView ? <App /> : <MultiServerApp />}
      </MantineProvider>
    </UIErrorBoundary>,
  );
}
