import { fixtureElement } from "./browser-helpers.ts";
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import App from "../../src/App.tsx";
import { SidebarWarningsProvider } from "../../src/components/SidebarWarnings.tsx";
import { theme } from "../../src/theme.ts";
import "../../src/style.css";
createRoot(fixtureElement("root")).render(
  React.createElement(
    MantineProvider,
    { theme },
    React.createElement(
      SidebarWarningsProvider,
      null,
      React.createElement(App),
    ),
  ),
);
