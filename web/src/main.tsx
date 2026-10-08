import {
  DesktopServerCredentials,
  nativeCredentialBridge,
} from "./servers/desktopCredentials";
import { BrowserServerCredentials } from "./servers/browserCredentials";
import { setServerCredentialAdapter } from "./servers/transport";
import { installFrameDesktopBridge } from "./servers/desktopBridge";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import App from "./App";
import MultiServerApp from "./servers/MultiServerApp";
import { isServerView } from "./servers/environment";
import "./servers/servers.css";
import UIErrorBoundary from "./components/UIErrorBoundary";
import { theme } from "./theme";
import "./style.css";
import "./workspace-layout.css";
import "./appearance.css";
import "./studio-preferences.css";

const nativeCredentials = nativeCredentialBridge();
setServerCredentialAdapter(
  nativeCredentials
    ? new DesktopServerCredentials(nativeCredentials)
    : new BrowserServerCredentials(),
);
installFrameDesktopBridge();
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
