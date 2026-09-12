import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import App from "./App";
import UIErrorBoundary from "./components/UIErrorBoundary";
import { theme } from "./theme";
import "./style.css";
import "./workspace-layout.css";
import "./appearance.css";

createRoot(document.getElementById("root")!).render(
  <UIErrorBoundary label="Studio" fullPage>
    <MantineProvider theme={theme} defaultColorScheme="auto">
      <App />
    </MantineProvider>
  </UIErrorBoundary>,
);
