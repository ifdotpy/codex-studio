import type { ReactNode } from "react";
import type { Account } from "../components/Accounts";

/** Account UI needs an exact projection feed and its matching service scope. */
export interface AccountServices {
  watchAccounts(
    receive: (accounts: Account[] | undefined) => void,
    failed: (error: unknown) => void,
  ): () => void;
  renderScope(children: ReactNode): ReactNode;
}
