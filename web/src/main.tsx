import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import App from "./App";
import UIErrorBoundary from "./components/UIErrorBoundary";
import { theme } from "./theme";
import "./style.css";
import "./workspace-layout.css";
import "./appearance.css";
import "./studio-preferences.css";

const container = document.getElementById("root")!;
if (container.dataset.studioMounted !== "true") {
  container.dataset.studioMounted = "true";
  createRoot(container).render(
    <UIErrorBoundary label="Studio" fullPage>
      <MantineProvider theme={theme} defaultColorScheme="auto">
        <App />
      </MantineProvider>
    </UIErrorBoundary>,
  );
}
