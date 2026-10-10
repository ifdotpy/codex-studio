import { post } from "../../api";
import Accounts, { type useAccounts } from "../Accounts";
import { ExecutionSettings } from "./ExecutionSettings";
import type { ComponentProps } from "react";

export function UnifiedAgentSettings({
  state,
  notify,
  onAccountModalOpenChange,
  ...settings
}: Omit<
  ComponentProps<typeof ExecutionSettings>,
  "onAccountChange" | "accountDisabled" | "accounts"
> & {
  state: ReturnType<typeof useAccounts>;
  notify: (message: string) => void;
  onAccountModalOpenChange?: (opened: boolean) => void;
}) {
  return (
    <Accounts
      state={state}
      onModalOpenChange={onAccountModalOpenChange}
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
        <ExecutionSettings
          {...settings}
          accounts={state.data.accounts}
          onAccountChange={selectAccount}
          accountDisabled={disabled}
        />
      )}
    />
  );
}
