import type { AccountsState } from "../../src/components/Accounts.tsx";
import { required } from "./fixture-helpers.ts";
const initial: AccountsState = await (await fetch("/fixture-data")).json();
declare global {
  interface Window {
    accountsCardsFixture: { setTheme(value: "dark" | "light"): void };
  }
}
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Modal, Tabs } from "@mantine/core";
import Accounts from "../../src/components/Accounts.tsx";
import { theme, modalSizes } from "../../src/theme.ts";
import "@mantine/core/styles.css";
import "../../src/style.css";
import "../../src/appearance.css";
function Fixture() {
  const [data, setData] = useState<AccountsState>(initial);
  const [color, setColor] = useState<"dark" | "light">("dark");
  const [blocked, setBlocked] = useState(false);
  window.accountsCardsFixture = { setTheme: setColor };
  return (
    <MantineProvider theme={theme} forceColorScheme={color}>
      <Modal
        opened
        title="Studio settings"
        size={modalSizes.settings}
        onClose={() => {}}
        closeOnEscape={!blocked}
        transitionProps={{ duration: 0 }}
        classNames={{ body: "studio-settings-body" }}
      >
        <div className="studio-settings-panel">
          <Tabs defaultValue="accounts" className="studio-settings-tabs">
            <Tabs.List aria-label="Studio settings">
              <Tabs.Tab value="accounts">Accounts</Tabs.Tab>
              <Tabs.Tab value="appearance">Appearance</Tabs.Tab>
            </Tabs.List>
            <Tabs.Panel value="accounts" pt="md">
              <section className="settings-group" aria-label="Studio accounts">
                <Accounts
                  managerOnly
                  state={{
                    data,
                    setData,
                    error: "",
                    scope: "cards",
                    refresh: async () => data,
                  }}
                  onError={() => {}}
                  onModalOpenChange={setBlocked}
                />
              </section>
            </Tabs.Panel>
          </Tabs>
        </div>
      </Modal>
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Fixture />);
