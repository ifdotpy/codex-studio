import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import App from "./App";
import { theme } from "./theme";
import "./style.css";
import "./workspace-layout.css";
import "./appearance.css";

createRoot(document.getElementById("root")!).render(
  <MantineProvider theme={theme} defaultColorScheme="auto">
    <App />
  </MantineProvider>,
);
