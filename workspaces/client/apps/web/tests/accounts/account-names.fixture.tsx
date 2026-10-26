import type { AccountsState } from "../../src/components/Accounts.tsx";
import { required } from "./fixture-helpers.ts";
const initial: AccountsState = await (await fetch("/fixture-data")).json();
declare global {
  interface Window {
    accountNamesFixture: { setTheme(value: "dark" | "light"): void };
  }
}
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import Accounts from "../../src/components/Accounts.tsx";
import { theme } from "../../src/theme.ts";
import "@mantine/core/styles.css";
import "../../src/style.css";
import "../../src/appearance.css";

function Fixture() {
  const [data, setData] = useState<AccountsState>(initial);
  const [color, setColor] = useState<"dark" | "light">("dark");
  window.accountNamesFixture = { setTheme: setColor };
  const state = {
    data,
    setData,
    scope: undefined,
    error: "",
    refresh: async () => data,
  };
  return (
    <MantineProvider theme={theme} forceColorScheme={color}>
      <main
        style={{ padding: 16, width: "100%", maxWidth: 760, overflow: "auto" }}
      >
        <Accounts state={state} managerOnly onError={() => {}} />
      </main>
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Fixture />);
