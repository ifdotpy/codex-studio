import { post } from "../../api";
import Accounts, { type useAccounts } from "../Accounts";
import { ExecutionSettings } from "./ExecutionSettings";
import { AccountExclusionsControl } from "./AccountExclusionsControl";
import { useCallback, useState, type ComponentProps } from "react";

export function UnifiedAgentSettings({
  state,
  notify,
  onAccountModalOpenChange,
  showAccountExclusions = false,
  ...settings
}: Omit<
  ComponentProps<typeof ExecutionSettings>,
  "onAccountChange" | "accountDisabled" | "accounts"
> & {
  state: ReturnType<typeof useAccounts>;
  notify: (message: string) => void;
  onAccountModalOpenChange?: (opened: boolean) => void;
  showAccountExclusions?: boolean;
}) {
  const [accountModalOpen, setAccountModalOpen] = useState(false);
  const updateAccountModalOpen = useCallback(
    (opened: boolean) => {
      setAccountModalOpen(opened);
      onAccountModalOpenChange?.(opened);
    },
    [onAccountModalOpenChange],
  );
  return (
    <Accounts
      state={state}
      onModalOpenChange={updateAccountModalOpen}
      agent={settings.agent}
      accountKey={settings.agent.accountKey || "default"}
      onError={notify}
      changeAccount={async (key) => {
        await post("/api/agents/account", {
          id: settings.agent.id,
          account_key: key,
        });
        await settings.refresh();
      }}
      renderPicker={(selectAccount, disabled) => (
        <>
          <ExecutionSettings
            {...settings}
            accounts={state.data.accounts}
            onAccountChange={selectAccount}
            accountDisabled={disabled}
            accountModalOpen={accountModalOpen}
          />
          {showAccountExclusions && (
            <AccountExclusionsControl
              lead={settings.agent}
              accounts={state.data}
              refresh={settings.refresh}
              onError={notify}
            />
          )}
        </>
      )}
    />
  );
}
