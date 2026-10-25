import ClaudeOrganization from "./ClaudeOrganization";
import Disclosure from "./ui/Disclosure";
import {
  useServerApi,
  useServerSync,
  useServerStorage,
} from "../servers/ServerServicesContext";
import { accountDisplayName, accountCanStart } from "../accountName";
import {
  ProviderMark,
  remainingText,
  useAccountLimitView,
  type AccountLimitView,
} from "./AccountTiles";
import { ActionButton } from "./ui/primitives";
import { localDateTime } from "../local-time";
import ErrorDescription from "./ErrorDescription";
import ClaudeProfile from "./ClaudeProfile";
import AccountStartThrottle from "./AccountStartThrottle";
import AccountGroup from "./AccountGroup";
import ClaudeSignIn from "./ClaudeSignIn";
import CodexSignIn from "./CodexSignIn";
import NativeRuntimeStatus from "./NativeRuntimeStatus";
import {
  Accordion,
  Button,
  Group,
  Menu,
  Modal,
  Progress,
  Stack,
  TextInput,
  Tooltip,
} from "@mantine/core";
import { layerZIndex } from "../theme";
import {
  Check,
  ArrowRightLeft,
  Plus,
  RefreshCw,
  Pencil,
  Ellipsis,
  CircleDot,
  LogIn,
  AlertCircle,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useMemo,
  useState,
  type SetStateAction,
  type ReactNode,
} from "react";
import { errorText, type GetResult } from "../api";
import type { Agent } from "../types";
import "./accounts.css";
import { jsonObject } from "../usage/accountUsage";
import { remainingLimitColor } from "./accountLimitColor";
import AccountSignIn from "./AccountSignIn";
import AccountManagerHost from "./AccountManagerHost";
import AccountAddDialog, { type AddAccountIntent } from "./AccountAddDialog";
import {
  accountForExternalCommand,
  confirmAccountDeleteRequest,
  getOrCreateAccountDeleteRequest,
} from "./accountDeleteRequests";
import type { ServerCommand } from "../servers/navigation";
import {
  closeMenuFromTriggerOnEscape,
  restoreMenuTriggerFocusAfterClose,
  useMenuEscapeSuppression,
} from "./closeMenuFromTriggerOnEscape";

export type AccountsState = GetResult<"/api/accounts">;
export type Account = AccountsState["accounts"][number];
export function useAccounts(stateDir?: string) {
  const { get } = useServerApi();
  const { subscribeStateProjection } = useServerSync();

  const [data, setAccountsData] = useState<AccountsState>({
    accounts: [],
    archivedAccounts: [],
    defaultAccountKey: "default",
    defaultAccountGroups: [],
    logins: [],
    supportsDelete: true,
    supportsDisconnect: true,
  });
  const [error, setError] = useState("");
  const latestRead = useRef(0);
  const scope = useRef(stateDir);
  scope.current = stateDir;
  const setData = useCallback((value: SetStateAction<AccountsState>) => {
    latestRead.current++;
    setAccountsData(value);
    setError("");
  }, []);
  const refresh = useCallback(async () => {
    const read = ++latestRead.current;
    try {
      const result = await get("/api/accounts");
      if (read !== latestRead.current || scope.current !== stateDir)
        return null;
      setAccountsData(result);
      setError("");
      return result;
    } catch (e) {
      if (read === latestRead.current && scope.current === stateDir)
        setError(errorText(e));
      return null;
    }
  }, [stateDir, get]);
  useEffect(() => {
    if (!stateDir) return;
    const stop = subscribeStateProjection(
      (snapshot) => {
        const summary = snapshot?.runtime.accountsSummary;
        if (summary)
          setAccountsData((previous) => ({
            ...summary,
            logins: previous.logins,
          }));
      },
      (failure) => {
        if (failure) setError(errorText(failure));
      },
    );
    return () => {
      latestRead.current++;
      stop();
    };
  }, [stateDir, subscribeStateProjection]);

  return useMemo(
    () => ({ data, setData, error, refresh, scope: stateDir }),
    [data, setData, error, refresh, stateDir],
  );
}

function AccountNameEditor({
  account,
  onSaved,
  disabled,
}: {
  account: Account;
  onSaved: (data: AccountsState) => void;
  disabled: boolean;
}) {
  const { post } = useServerApi();

  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(account.label);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const request = useRef<{ label: string; request_id: string } | null>(null);
  const lock = useRef(false);
  const fallback = account.email?.split("@")[0] || "Account";
  const nameTooLong = [...name.trim()].length > 32;
  const save = async () => {
    if (lock.current || disabled) return;
    const label = name.trim();
    if ([...label].length > 32) {
      setError("Use at most 32 characters.");
      return;
    }
    if (request.current?.label !== label)
      request.current = { label, request_id: crypto.randomUUID() };
    lock.current = true;
    setBusy(true);
    setError("");
    try {
      const result = await post("/api/accounts/name", {
        account_key: account.id,
        ...request.current,
      });
      onSaved(result);
      request.current = null;
      setEditing(false);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  };
  const cancel = () => {
    if (lock.current) return;
    setEditing(false);
    setError("");
  };
  return (
    <div className="account-name">
      {editing ? (
        <form
          className="account-name-form"
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <TextInput
            label="Name"
            aria-label={`Name for ${account.email || account.id}`}
            placeholder={fallback}
            value={name}
            onChange={(event) => setName(event.currentTarget.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                event.stopPropagation();
                cancel();
              }
            }}
            disabled={busy || disabled}
            autoFocus
            error={
              error || (nameTooLong ? "Use at most 32 characters." : undefined)
            }
          />
          <div className="account-name-actions">
            <ActionButton
              type="submit"
              actionRole="primary"
              loading={busy}
              disabled={disabled || nameTooLong}
            >
              Save
            </ActionButton>
            <ActionButton actionRole="quiet" onClick={cancel} disabled={busy}>
              Cancel
            </ActionButton>
          </div>
        </form>
      ) : (
        <>
          <ActionButton
            actionRole="quiet"
            leftSection={<Pencil size={14} />}
            aria-label={`Edit name for ${account.email || account.id}`}
            disabled={disabled}
            onClick={() => {
              setName(account.label);
              setError("");
              setEditing(true);
            }}
          >
            {accountDisplayName(account)}
          </ActionButton>
        </>
      )}
    </div>
  );
}

export function AccountCapacity({
  account,
  opened,
  card = false,
  extraDetails,
  facts,
}: {
  account: Account;
  opened: boolean;
  card?: boolean;
  extraDetails?: ReactNode;
  facts?: ReactNode;
}) {
  const view = useAccountLimitView(account, { read: opened });
  if (account.status !== "ready" && !card) return null;
  return (
    <AccountCapacityView
      account={account}
      view={view}
      card={card}
      extraDetails={extraDetails}
      facts={facts}
    />
  );
}

/** One reading of an account's provider limits. `note` says where and when
 * the reading came from when the account has more than one source. */
export function AccountCapacityView({
  account,
  view,
  card = false,
  note,
  extraDetails,
  facts,
}: {
  account: Pick<Account, "status" | "email" | "label">;
  view: AccountLimitView;
  card?: boolean;
  note?: ReactNode;
  extraDetails?: ReactNode;
  facts?: ReactNode;
}) {
  const { limits, buckets, remaining, error: limitsError } = view;
  const credits = jsonObject(jsonObject(limits?.data)?.rateLimitResetCredits);
  const availableCredits = Array.isArray(credits?.credits)
    ? credits.credits.flatMap((value) => {
        const credit = jsonObject(value);
        return credit?.status === "available" ? [credit] : [];
      })
    : [];
  const windows = (
    <div className="account-window-grid">
      <table className="account-limits-table">
        <thead>
          <tr>
            <th>Window</th>
            <th>Meter</th>
            <th>Left</th>
            <th>Resets at</th>
          </tr>
        </thead>
        <tbody>
          {buckets.flatMap((bucket) =>
            bucket.windows.map((window, index) => (
              <tr key={`${bucket.id}-${index}`}>
                <th scope="row">
                  <span>
                    {/spark/i.test(bucket.name) ? "Spark" : bucket.name}
                  </span>
                  <small>{window.label}</small>
                </th>
                <td>
                  {window.remaining !== null && !window.expired ? (
                    <Progress.Root
                      className="account-window-meter"
                      size="xs"
                      role="meter"
                      aria-label={`${bucket.name} ${window.label} remaining`}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={window.remaining}
                    >
                      <Progress.Section
                        value={window.remaining}
                        color={remainingLimitColor(window.remaining)}
                        withAria={false}
                      />
                    </Progress.Root>
                  ) : (
                    <span>—</span>
                  )}
                </td>
                <td>
                  {window.expired
                    ? "Reset due"
                    : window.remaining === null
                      ? "Unknown"
                      : remainingText(window.remaining)}
                </td>
                <td>
                  {window.reset ? (
                    <time
                      dateTime={new Date(window.reset * 1000).toISOString()}
                      title={localDateTime(new Date(window.reset * 1000))}
                    >
                      {localDateTime(new Date(window.reset * 1000), {
                        month: "short",
                        day: "numeric",
                        hour: "2-digit",
                        minute: "2-digit",
                      })}
                    </time>
                  ) : (
                    "Unavailable"
                  )}
                </td>
              </tr>
            )),
          )}
          {!buckets.length && (
            <tr>
              <td colSpan={4}>
                {limitsError
                  ? "Limits unavailable"
                  : view.reading
                    ? "Reading limits…"
                    : "Limits unavailable"}
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
  const errors = limitsError && (
    <ErrorDescription
      className="account-action-error"
      role="status"
      value={limits?.readError || limitsError}
    />
  );
  if (!card)
    return (
      <div className="account-capacity">
        {errors}
        {windows}
      </div>
    );
  return (
    <div
      className="account-capacity"
      aria-label={`Limits for ${account.email || account.label}`}
    >
      <div className="account-limit-summary">
        <span>Remaining limit</span>
        <span
          role={remaining === null ? undefined : "meter"}
          aria-label="Remaining limit"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={remaining ?? undefined}
        >
          <i
            className="account-summary-dot"
            aria-hidden="true"
            style={{ background: remainingLimitColor(remaining) }}
          />
          {remaining === null
            ? view.reading
              ? "Reading limits…"
              : "Unavailable"
            : remainingText(remaining)}
        </span>
      </div>
      <Accordion.Panel className="account-expanded">
        <section className="account-detail-block">
          <h4>Limits</h4>
          {note && <small className="account-limit-note">{note}</small>}
          {errors}
          {windows}
        </section>
        <section className="account-detail-block">
          <h4>Resets</h4>
          <p>
            {typeof credits?.availableCount === "number"
              ? `${credits.availableCount} ${credits.availableCount === 1 ? "credit" : "credits"} available`
              : "No reset credits reported."}
          </p>
          {availableCredits.map(
            (credit, index) =>
              typeof credit.expiresAt === "number" && (
                <p key={index}>
                  Expires{" "}
                  <time
                    dateTime={new Date(credit.expiresAt * 1000).toISOString()}
                  >
                    {localDateTime(new Date(credit.expiresAt * 1000))}
                  </time>
                </p>
              ),
          )}
          <small>Redeem reset credits from Limits in a chat.</small>
        </section>
        <section className="account-detail-block">
          <h4>Account</h4>
          {facts || (
            <dl className="account-facts">
              <dt>Email</dt>
              <dd>{account.email || "Unavailable"}</dd>
              <dt>Sign-in</dt>
              <dd>
                {account.status === "ready" ? "Signed in" : "Sign in needed"}
              </dd>
            </dl>
          )}
        </section>
        {extraDetails && (
          <section className="account-detail-block">
            <h4>Actions</h4>
            {extraDetails}
          </section>
        )}
      </Accordion.Panel>
    </div>
  );
}

export function AccountTransferStatus({
  transfer,
  targetLabel,
  pending,
  showCompleted = false,
  onAction,
}: {
  transfer?: NonNullable<Agent["accountTransfer"]>;
  targetLabel?: string;
  pending?: boolean;
  showCompleted?: boolean;
  onAction: (action: "retry" | "cancel") => void;
}) {
  if (!transfer) return null;
  const waiting = Number(transfer.waitingCount || 0);
  const left = Array.isArray(transfer.leftOnSource)
    ? transfer.leftOnSource
    : [];
  // One line per reason: a team move can leave many subagents for one reason.
  const leftGroups = [
    ...left
      .reduce(
        (groups, member) => {
          const key = JSON.stringify([member.provider, member.reason]);
          const group = groups.get(key) || {
            key,
            provider: member.provider,
            reason: member.reason,
            names: [] as string[],
          };
          group.names.push(String(member.name || "Agent"));
          return groups.set(key, group);
        },
        new Map<
          string,
          {
            key: string;
            provider: (typeof left)[number]["provider"];
            reason: (typeof left)[number]["reason"];
            names: string[];
          }
        >(),
      )
      .values(),
  ];
  const blocked = Array.isArray(transfer.blocked) ? transfer.blocked : [];
  const interrupted = Array.isArray(transfer.interrupted)
    ? transfer.interrupted
    : [];
  if (
    transfer.status !== "pending" &&
    !left.length &&
    !blocked.length &&
    (!showCompleted ||
      !Number(transfer.moved ?? transfer.completed ?? 0) ||
      !targetLabel)
  )
    return null;
  return (
    <div className="account-menu-note" role="status">
      <ArrowRightLeft size={14} />
      <div>
        {Number(transfer.moved ?? transfer.completed ?? 0) > 0 &&
          targetLabel && (
            <div>
              Moved {transfer.moved ?? transfer.completed} subagents to{" "}
              {targetLabel}.
            </div>
          )}
        {transfer.status === "pending" && waiting > 0 && (
          <div>{waiting} waiting</div>
        )}
        {!!transfer.nativeHistoryPending && (
          <small>
            History transfer in progress: {transfer.nativeHistoryPending}{" "}
            remaining.
          </small>
        )}
        {!!transfer.movingNow && <small>{transfer.movingNow} moving now</small>}
        {transfer.waiting && <small>{transfer.waiting}</small>}
        {interrupted.map((member) => (
          <small key={String(member.id)}>
            {member.name || "Agent"}: {member.reason}
          </small>
        ))}
        {blocked.map((member) => (
          <small key={String(member.id)} role="alert">
            {member.name || "Agent"} blocked: {member.reason}
          </small>
        ))}
        {leftGroups.map((group) => (
          <small key={group.key}>
            {group.names.join(", ")} left on source ({group.provider}):{" "}
            {group.reason}
          </small>
        ))}
        <div className="account-transfer-actions">
          {transfer.canRetry && (
            <Button
              size="compact-xs"
              variant="subtle"
              disabled={pending}
              onClick={() => onAction("retry")}
            >
              Retry
            </Button>
          )}
          {transfer.status === "pending" && (
            <Button
              size="compact-xs"
              variant="subtle"
              // A subagent whose history is moving now finishes first; the server
              // refuses a cancel until then.
              disabled={pending || Number(transfer.movingNow || 0) > 0}
              title={
                Number(transfer.movingNow || 0) > 0
                  ? "Cancel after the subagent that is moving now finishes."
                  : undefined
              }
              onClick={() => onAction("cancel")}
            >
              Cancel remaining
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

/** One line next to the model picker while a team moves to another account. The details,
 * with Retry and Cancel, are in the model settings; the tooltip shows the same reasons. */
export function AccountTransferChip({
  transfer,
}: {
  transfer?: NonNullable<Agent["accountTransfer"]>;
}) {
  if (!transfer || transfer.status !== "pending") return null;
  const blocked = Array.isArray(transfer.blocked) ? transfer.blocked : [];
  const left =
    Number(transfer.total ?? 0) - Number(transfer.completed ?? 0) ||
    Number(transfer.nativeHistoryPending ?? 0);
  const label = blocked.length
    ? "Account move blocked"
    : `Moving account${left > 0 ? ` · ${left} left` : ""}`;
  const details = [
    ...blocked.map((member) => `${member.name || "Agent"}: ${member.reason}`),
    ...(transfer.waiting && !blocked.length ? [String(transfer.waiting)] : []),
    "Open the model settings to retry or cancel.",
  ].join("\n");
  return (
    <Tooltip label={details} multiline w={320} withinPortal>
      <span
        className="account-transfer-chip"
        data-blocked={blocked.length ? "" : undefined}
        role="status"
        aria-label={`${label}. ${details}`}
      >
        <ArrowRightLeft size={12} />
        {label}
      </span>
    </Tooltip>
  );
}

export function AccountTransferConfirmation({
  opened,
  onClose,
  target,
  sourceProvider,
  extend = false,
  onConfirm,
}: {
  opened: boolean;
  onClose: () => void;
  target: Account | null;
  sourceProvider: string;
  extend?: boolean;
  onConfirm: () => Promise<void>;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  useEffect(() => {
    setError("");
  }, [target?.id, opened]);
  return (
    <Modal
      opened={opened}
      zIndex={layerZIndex.modalAbovePopover}
      onClose={() => {
        if (!pending) onClose();
      }}
      title={extend ? "Add agents to this transfer" : "Transfer this chat"}
      closeOnClickOutside={!pending}
      closeOnEscape={!pending}
      withCloseButton={!pending}
    >
      <p>
        {extend
          ? "Add descendants to the pending team transfer to"
          : "Transfer this chat to"}{" "}
        <strong>{target?.email || target?.label}</strong>?
      </p>
      <p>
        Active turns move immediately and continue on the destination. Same
        provider subagents move too; other providers stay on their accounts.
      </p>
      {target && sourceProvider !== (target.provider || "codex") && (
        <p>The destination model uses the saved chat context.</p>
      )}
      <p>Applies to this chat only.</p>
      {error && <p role="alert">{error}</p>}
      <Button variant="default" disabled={pending} onClick={onClose}>
        Cancel
      </Button>
      <Button
        loading={pending}
        disabled={!target || pending}
        onClick={async () => {
          if (lock.current) return;
          lock.current = true;
          setPending(true);
          setError("");
          try {
            await onConfirm();
            onClose();
          } catch (failure) {
            setError(errorText(failure));
          } finally {
            lock.current = false;
            setPending(false);
          }
        }}
      >
        {extend ? "Add agents" : "Transfer chat"}
      </Button>
    </Modal>
  );
}

export default function Accounts({
  state,
  agent,
  accountKey,
  changeAccount,
  onError,
  onModalOpenChange,
  managerOnly = false,
  renderPicker,
  externalCommand,
  onExternalCommandHandled,
  dialogsOnly = false,
  showTransferStatus = true,
}: {
  state: ReturnType<typeof useAccounts>;
  agent?: Agent;
  accountKey?: string;
  onModalOpenChange?: (opened: boolean) => void;
  managerOnly?: boolean;
  renderPicker?: (
    selectAccount: (key: string) => void,
    disabled: boolean,
  ) => ReactNode;
  changeAccount?: (key: string) => Promise<void>;
  onError: (message: string) => void;
  externalCommand?: Extract<
    ServerCommand,
    { action: "account-sign-in" | "account-action" }
  > | null;
  onExternalCommandHandled?: () => void;
  dialogsOnly?: boolean;
  showTransferStatus?: boolean;
}) {
  const { post } = useServerApi();
  const storage = useServerStorage();

  const [opened, setOpened] = useState(false);
  const [openActionMenuAccountId, setOpenActionMenuAccountId] = useState<
    string | null
  >(null);
  const actionMenuTriggers = useRef(new Map<string, HTMLButtonElement>());
  const { suppressNextEscape, clearSuppression } = useMenuEscapeSuppression(
    openActionMenuAccountId !== null,
  );
  const [pending, setPending] = useState("");
  const [error, setError] = useState("");
  const [home, setHome] = useState("");
  const [adding, setAdding] = useState(false);
  const [claudeLogin, setClaudeLogin] = useState<Account | null>(null);
  const [codexLogin, setCodexLogin] = useState<Account | null>(null);
  const [transferChoice, setTransferChoice] = useState<{
    target: Account;
    sourceProvider: string;
    agentId: string;
    requestId: string;
    extend: boolean;
  } | null>(null);
  const [disconnectChoice, setDisconnectChoice] = useState<Account | null>(
    null,
  );
  const [deleteChoice, setDeleteChoice] = useState<Account | null>(null);
  const [deleteRequestId, setDeleteRequestId] = useState("");
  const [addOpen, setAddOpen] = useState(false);
  const [addIntent, setAddIntent] = useState<AddAccountIntent | null>(null);
  const [externalServerLabel, setExternalServerLabel] = useState("");
  const [renameChoice, setRenameChoice] = useState<Account | null>(null);
  const [claudeProfileChoice, setClaudeProfileChoice] =
    useState<Account | null>(null);
  const [renameLabel, setRenameLabel] = useState("");
  const handledCommand = useRef<ServerCommand | null>(null);
  const actionLock = useRef(false);
  const childModalOpen =
    (!managerOnly && opened) ||
    addOpen ||
    !!renameChoice ||
    !!claudeProfileChoice ||
    !!transferChoice ||
    !!disconnectChoice ||
    !!deleteChoice ||
    !!claudeLogin ||
    !!codexLogin;
  useLayoutEffect(() => {
    onModalOpenChange?.(
      managerOnly
        ? !!addOpen ||
            !!renameChoice ||
            !!claudeProfileChoice ||
            !!disconnectChoice ||
            !!deleteChoice ||
            !!claudeLogin ||
            !!codexLogin
        : childModalOpen,
    );
    return () => onModalOpenChange?.(false);
  }, [
    childModalOpen,
    managerOnly,
    disconnectChoice,
    deleteChoice,
    claudeLogin,
    codexLogin,
    addOpen,
    renameChoice,
    claudeProfileChoice,
    onModalOpenChange,
  ]);
  const accounts = state.data.accounts || [];
  const selected =
    accounts.find((a) => a.id === accountKey) ||
    state.data.archivedAccounts?.find((a) => a.id === accountKey);
  const pinned =
    !!agent &&
    (!agent.isLead || !agent.empty || !!agent.threadId || !!agent.inFlight);
  const owner = agent?.isLead ? agent : undefined;
  const transfer = owner?.accountTransfer;
  const teamTransfer = transfer?.scope === "subagents" ? undefined : transfer;
  const transferring = teamTransfer?.status === "pending";
  const replacement = accounts.find(
    (account) =>
      account.id !== disconnectChoice?.id &&
      !account.disconnected &&
      account.status === "ready",
  );
  const disconnectsDefault =
    disconnectChoice?.id === state.data.defaultAccountKey;
  const deletesDefault = deleteChoice?.id === state.data.defaultAccountKey;
  const deleteReplacement = accounts.find(
    (account) =>
      account.id !== deleteChoice?.id &&
      !account.disconnected &&
      account.status === "ready",
  );
  const action = async (key: string, run: () => Promise<unknown>) => {
    if (actionLock.current) return;
    actionLock.current = true;
    setPending(key);
    setError("");
    try {
      await run();
    } catch (e) {
      setError(errorText(e));
      onError(errorText(e));
    } finally {
      actionLock.current = false;
      setPending("");
    }
  };
  useEffect(() => {
    if (!externalCommand || handledCommand.current === externalCommand) return;
    const command = externalCommand;
    handledCommand.current = command;
    setExternalServerLabel(
      command.action === "account-sign-in" ? command.serverLabel : "",
    );
    if (command.action === "account-sign-in" && command.forceAdd) {
      setAddIntent({
        provider: command.provider,
        email: command.email,
        label: command.label,
        serverLabel: command.serverLabel,
      });
      setAddOpen(true);
      onExternalCommandHandled?.();
      return;
    }
    if (
      command.action === "account-action" &&
      command.operation === "configure-profile"
    ) {
      const account = state.data.accounts.find(
        (candidate) =>
          candidate.provider === "claude" &&
          (command.email
            ? candidate.email?.toLocaleLowerCase() ===
              command.email.toLocaleLowerCase()
            : candidate.label === command.label),
      );
      if (account) setClaudeProfileChoice(account);
      else setError("This Claude account is no longer saved on this server.");
      onExternalCommandHandled?.();
      return;
    }
    void state.refresh().then((fresh) => {
      const current =
        fresh || (command.action === "account-sign-in" ? state.data : null);
      if (!current) {
        setError("Cannot load accounts from this server.");
        onExternalCommandHandled?.();
        return;
      }
      const account = accountForExternalCommand(current.accounts, command);
      if (command.action === "account-sign-in") {
        if (account && !command.forceAdd) {
          if (account.provider === "claude") setClaudeLogin(account);
          else setCodexLogin(account);
        } else {
          setAddIntent({
            provider: command.provider,
            email: command.email,
            label: command.label,
            serverLabel: command.serverLabel,
          });
          setAddOpen(true);
        }
      } else if (!account) {
        setError("This account is no longer saved on this server.");
      } else if (command.operation === "rename") {
        setRenameChoice(account);
        setRenameLabel(account.label);
      } else if (command.operation === "default") {
        void action(`default:${account.id}`, async () => {
          state.setData(
            await post("/api/accounts/default", { account_key: account.id }),
          );
        });
      } else if (command.operation === "disconnect") {
        setDisconnectChoice(account);
      } else {
        const requestId = getOrCreateAccountDeleteRequest(
          state.scope,
          account.id,
          storage,
        );
        setDeleteRequestId(requestId);
        setDeleteChoice(account);
      }
      onExternalCommandHandled?.();
    });
  }, [externalCommand]);
  const selectAccount = (key: string) => {
    const account = accounts.find((candidate) => candidate.id === key);
    if (
      !account ||
      actionLock.current ||
      pending ||
      !accountCanStart(account) ||
      (pinned && !owner) ||
      (transferring && account.id !== teamTransfer?.targetAccountKey)
    )
      return;
    const extendTransfer =
      transferring && account.id === teamTransfer?.targetAccountKey;
    if (account.id === accountKey && !extendTransfer) return;
    if (pinned && owner) {
      onModalOpenChange?.(true);
      setTransferChoice({
        target: account,
        sourceProvider: selected?.provider || owner.provider || "codex",
        agentId: owner.id,
        requestId: crypto.randomUUID(),
        extend: !!extendTransfer,
      });
    } else if (!pinned)
      void action(account.id, async () => {
        await changeAccount?.(account.id);
      });
  };
  return (
    <>
      {codexLogin && (
        <CodexSignIn
          account={codexLogin}
          state={state}
          serverLabel={externalServerLabel || undefined}
          onClose={() => setCodexLogin(null)}
        />
      )}
      {claudeLogin && (
        <ClaudeSignIn
          key={claudeLogin.id}
          account={claudeLogin}
          scope={state.scope}
          serverLabel={externalServerLabel || undefined}
          onClose={() => setClaudeLogin(null)}
          onReady={async () => {
            if (claudeLogin.disconnected)
              state.setData(
                await post("/api/accounts/reconnect", {
                  account_key: claudeLogin.id,
                }),
              );
            else await state.refresh();
          }}
        />
      )}
      {!managerOnly &&
        renderPicker &&
        renderPicker(selectAccount, !!pending || (pinned && !owner))}
      {!managerOnly && renderPicker && showTransferStatus && (
        <AccountTransferChip transfer={teamTransfer ?? undefined} />
      )}
      {!dialogsOnly && (
        <AccountManagerHost
          inline={managerOnly}
          opened={opened}
          onClose={() => setOpened(false)}
          title="Accounts"
          closeBlocked={!!disconnectChoice || !!deleteChoice || !!claudeLogin}
        >
          {adding ? (
            <>
              <div className="accounts-add-header">
                <h3>Add account</h3>
                <Button variant="subtle" onClick={() => setAdding(false)}>
                  Back to accounts
                </Button>
              </div>
              <AccountSignIn state={state} />
            </>
          ) : (
            <div className="accounts-setup">
              {managerOnly && <h3>Accounts</h3>}
              {!accounts.length && (
                <p className="accounts-intro">
                  No accounts found. Sign in or find profiles on this computer.
                </p>
              )}
              <div className="accounts-actions">
                <Button
                  variant="filled"
                  onClick={() => {
                    setAddIntent(null);
                    setAddOpen(true);
                  }}
                  leftSection={<Plus size={14} />}
                >
                  Add account
                </Button>
                <Button
                  variant="default"
                  leftSection={<RefreshCw size={14} />}
                  loading={pending === "discover"}
                  disabled={!!pending}
                  onClick={() =>
                    void action("discover", async () => {
                      state.setData(await post("/api/accounts/discover", {}));
                    })
                  }
                >
                  Find existing accounts
                </Button>
              </div>
            </div>
          )}
          {!adding && (
            <>
              <p className="accounts-intro">
                New chats use the application default unless the project has its
                own default. Existing chats keep their account.
              </p>
              <Accordion
                className="accounts-list"
                aria-label="Saved accounts"
                keepMounted={false}
                transitionDuration={0}
                chevronPosition="left"
              >
                {(["codex", "claude"] as const).map((provider) => {
                  const providerAccounts = accounts.filter(
                    (account) => (account.provider || "codex") === provider,
                  );
                  return providerAccounts.length ? (
                    <section
                      className="accounts-provider"
                      key={provider}
                      aria-label={`${provider === "claude" ? "Claude" : "Codex"} accounts`}
                    >
                      <h3>
                        <ProviderMark provider={provider} />
                        {provider === "claude" ? "Claude" : "Codex"}
                      </h3>
                      <div className="accounts-grid">
                        {providerAccounts.map((account) => (
                          <Accordion.Item
                            value={account.id}
                            className="account-row"
                            key={account.id}
                            data-account={account.id}
                            aria-label={account.email || account.label}
                          >
                            <div className="account-card-heading">
                              <Accordion.Control
                                aria-label={`Limit details for ${account.email || account.label}`}
                              >
                                <span className="account-identity">
                                  <span
                                    role="img"
                                    aria-label={
                                      provider === "claude" ? "Claude" : "Codex"
                                    }
                                  >
                                    <ProviderMark provider={provider} />
                                  </span>
                                  <strong
                                    title={account.email || account.label}
                                  >
                                    {accountDisplayName(account)}
                                  </strong>
                                </span>
                              </Accordion.Control>
                              {account.plan && (
                                <span className="account-plan">
                                  {account.plan}
                                </span>
                              )}
                              <span className="account-status-markers">
                                {(account.error ||
                                  account.status !== "ready") && (
                                  <span
                                    className="account-strip-state"
                                    data-error={
                                      Boolean(account.error) ||
                                      account.status === "error" ||
                                      undefined
                                    }
                                  >
                                    {account.error ||
                                    account.status === "error" ? (
                                      <AlertCircle
                                        size={14}
                                        aria-hidden="true"
                                      />
                                    ) : (
                                      <LogIn size={14} aria-hidden="true" />
                                    )}
                                    <span className="account-strip-state-label">
                                      {account.error ||
                                      account.status === "error"
                                        ? "Error"
                                        : "Sign in needed"}
                                    </span>
                                  </span>
                                )}
                                {account.id ===
                                  state.data.defaultAccountKey && (
                                  <span
                                    className="account-default"
                                    title="Application default"
                                  >
                                    <Check size={12} />
                                    <span className="sr-only">
                                      Application default
                                    </span>
                                  </span>
                                )}
                                {account.id === accountKey && (
                                  <span
                                    className="account-strip-state"
                                    title="Current account"
                                  >
                                    <CircleDot size={14} aria-hidden="true" />
                                    <span className="sr-only">
                                      Current account
                                    </span>
                                  </span>
                                )}
                              </span>
                              <Menu
                                opened={openActionMenuAccountId === account.id}
                                onChange={(nextOpened) => {
                                  setOpenActionMenuAccountId((current) =>
                                    nextOpened
                                      ? account.id
                                      : current === account.id
                                        ? null
                                        : current,
                                  );
                                  if (!nextOpened)
                                    restoreMenuTriggerFocusAfterClose(
                                      actionMenuTriggers.current.get(
                                        account.id,
                                      ) ?? null,
                                    );
                                }}
                                position="bottom-end"
                                withinPortal={false}
                                withInitialFocusPlaceholder={false}
                              >
                                <Menu.Target>
                                  <ActionButton
                                    ref={(node) => {
                                      if (node)
                                        actionMenuTriggers.current.set(
                                          account.id,
                                          node,
                                        );
                                      else
                                        actionMenuTriggers.current.delete(
                                          account.id,
                                        );
                                    }}
                                    actionRole="quiet"
                                    className="account-card-menu"
                                    onClick={(event) =>
                                      event.currentTarget.focus({
                                        preventScroll: true,
                                      })
                                    }
                                    loading={
                                      pending === `default:${account.id}`
                                    }
                                    data-mantine-stop-propagation={
                                      openActionMenuAccountId === account.id ||
                                      suppressNextEscape
                                        ? "true"
                                        : undefined
                                    }
                                    onKeyDown={(event) =>
                                      closeMenuFromTriggerOnEscape(
                                        event,
                                        openActionMenuAccountId === account.id,
                                        () =>
                                          setOpenActionMenuAccountId(
                                            (current) =>
                                              current === account.id
                                                ? null
                                                : current,
                                          ),
                                        suppressNextEscape,
                                        clearSuppression,
                                      )
                                    }
                                    onBlur={clearSuppression}
                                    aria-label={`Actions for ${account.email || account.label}`}
                                  >
                                    <Ellipsis size={18} />
                                  </ActionButton>
                                </Menu.Target>
                                <Menu.Dropdown data-mantine-stop-propagation="true">
                                  {account.id !==
                                    state.data.defaultAccountKey && (
                                    <Menu.Item
                                      disabled={
                                        !!pending || !accountCanStart(account)
                                      }
                                      aria-label={`Use ${account.email || account.label} by default`}
                                      onClick={() =>
                                        void action(
                                          `default:${account.id}`,
                                          async () => {
                                            state.setData(
                                              await post(
                                                "/api/accounts/default",
                                                {
                                                  account_key: account.id,
                                                },
                                              ),
                                            );
                                          },
                                        )
                                      }
                                    >
                                      Set application default
                                    </Menu.Item>
                                  )}
                                  {account.provider === "claude" && (
                                    <>
                                      <Menu.Item
                                        onClick={() => setClaudeLogin(account)}
                                      >
                                        Sign in again
                                      </Menu.Item>
                                    </>
                                  )}
                                  {account.provider !== "claude" &&
                                    account.accountId && (
                                      <Menu.Item
                                        onClick={() => setCodexLogin(account)}
                                      >
                                        Sign in again
                                      </Menu.Item>
                                    )}
                                  {(state.data.supportsDisconnect ||
                                    state.data.supportsDelete) && (
                                    <Menu.Divider />
                                  )}
                                  {state.data.supportsDisconnect && (
                                    <Menu.Item
                                      color={
                                        account.disconnected ? "gray" : "red"
                                      }
                                      className={
                                        account.disconnected
                                          ? undefined
                                          : "account-destructive-action"
                                      }
                                      disabled={!!pending}
                                      onClick={() => {
                                        if (account.disconnected)
                                          void action(
                                            `reconnect:${account.id}`,
                                            async () => {
                                              state.setData(
                                                await post(
                                                  "/api/accounts/reconnect",
                                                  {
                                                    account_key: account.id,
                                                  },
                                                ),
                                              );
                                            },
                                          );
                                        else setDisconnectChoice(account);
                                      }}
                                    >
                                      {account.disconnected
                                        ? "Reconnect account"
                                        : "Disconnect account"}
                                    </Menu.Item>
                                  )}
                                  {state.data.supportsDelete && (
                                    <Menu.Item
                                      color="red"
                                      className="account-destructive-action"
                                      disabled={!!pending}
                                      onClick={() => {
                                        const requestId =
                                          getOrCreateAccountDeleteRequest(
                                            state.scope,
                                            account.id,
                                            storage,
                                          );
                                        setDeleteRequestId(requestId);
                                        setDeleteChoice(account);
                                      }}
                                    >
                                      Delete account
                                    </Menu.Item>
                                  )}
                                </Menu.Dropdown>
                              </Menu>
                            </div>
                            <AccountCapacity
                              card
                              account={account}
                              opened={
                                !account.disconnected && (managerOnly || opened)
                              }
                              facts={
                                <dl className="account-facts">
                                  <dt>Email</dt>
                                  <dd>{account.email || "Unavailable"}</dd>
                                  <dt>Server</dt>
                                  <dd>This computer</dd>
                                  <dt>Default</dt>
                                  <dd>
                                    {account.id === state.data.defaultAccountKey
                                      ? "Application default"
                                      : "No"}
                                  </dd>
                                  <dt>Sign-in</dt>
                                  <dd>
                                    {account.error || account.status === "error"
                                      ? "Error"
                                      : account.status === "ready"
                                        ? "Signed in"
                                        : "Sign in needed"}
                                  </dd>
                                </dl>
                              }
                              extraDetails={
                                <>
                                  <ClaudeOrganization account={account} />
                                  <AccountNameEditor
                                    account={account}
                                    onSaved={state.setData}
                                    disabled={!!pending}
                                  />

                                  {Boolean(account.error) && (
                                    <ErrorDescription
                                      className="account-action-error"
                                      value={account.error}
                                    />
                                  )}
                                  {account.authenticationRecovery && (
                                    <p role="alert">
                                      {account.authenticationRecovery}
                                    </p>
                                  )}
                                  {account.disconnected && (
                                    <p>Hidden from new chats.</p>
                                  )}
                                  {!account.disconnected && (
                                    <AccountStartThrottle
                                      account={account}
                                      onSaved={state.setData}
                                    />
                                  )}
                                  {!account.disconnected && (
                                    <AccountGroup
                                      account={account}
                                      onSaved={state.setData}
                                    />
                                  )}
                                  {account.provider === "claude" && (
                                    <ClaudeProfile
                                      account={account}
                                      onSaved={state.setData}
                                    />
                                  )}
                                </>
                              }
                            />
                          </Accordion.Item>
                        ))}
                      </div>
                    </section>
                  ) : null;
                })}
              </Accordion>
              <NativeRuntimeStatus
                opened={managerOnly || opened}
                accounts={accounts}
              />
            </>
          )}
          {adding && <ClaudeProfile onSaved={state.setData} />}
          {adding && (
            <Disclosure
              className="account-register"
              summary="Add a Codex home directory"
            >
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  void action("register", async () => {
                    state.setData(
                      await post("/api/accounts/register", {
                        home: home.trim(),
                      }),
                    );
                    setHome("");
                  });
                }}
              >
                <TextInput
                  label="Codex home"
                  description="Enter the path to the existing .codex directory for this account."
                  placeholder="/path/to/.codex"
                  value={home}
                  onChange={(e) => setHome(e.target.value)}
                  autoComplete="off"
                />
                <Button
                  type="submit"
                  size="compact-sm"
                  disabled={!home.trim() || !!pending}
                  loading={pending === "register"}
                >
                  Add profile
                </Button>
              </form>
            </Disclosure>
          )}
          {(error || state.error) && (
            <p className="account-action-error" role="alert">
              {error || state.error}
            </p>
          )}
        </AccountManagerHost>
      )}
      <AccountAddDialog
        opened={addOpen}
        intent={addIntent}
        state={state}
        onClose={() => setAddOpen(false)}
      />
      <Modal
        opened={!!renameChoice}
        zIndex={layerZIndex.modalAbovePopover}
        onClose={() => setRenameChoice(null)}
        title={`Rename ${renameChoice?.email || renameChoice?.label || "account"}`}
        centered
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!renameChoice) return;
            void action(`rename:${renameChoice.id}`, async () => {
              state.setData(
                await post("/api/accounts/name", {
                  account_key: renameChoice.id,
                  label: renameLabel,
                  request_id: crypto.randomUUID(),
                }),
              );
              setRenameChoice(null);
            });
          }}
        >
          <Stack gap="sm">
            <TextInput
              label="Name"
              value={renameLabel}
              maxLength={32}
              onChange={(event) => setRenameLabel(event.currentTarget.value)}
            />
            <Group justify="flex-end">
              <Button variant="default" onClick={() => setRenameChoice(null)}>
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={!renameLabel.trim() || !!pending}
                loading={pending === `rename:${renameChoice?.id}`}
              >
                Save
              </Button>
            </Group>
          </Stack>
        </form>
      </Modal>
      <Modal
        opened={!!claudeProfileChoice}
        zIndex={layerZIndex.modalAbovePopover}
        onClose={() => setClaudeProfileChoice(null)}
        title="Configure Claude"
        centered
      >
        {claudeProfileChoice && (
          <ClaudeProfile
            account={claudeProfileChoice}
            expanded
            onSaved={(value) => {
              state.setData(value);
            }}
          />
        )}
      </Modal>
      <AccountTransferConfirmation
        opened={!!transferChoice}
        target={transferChoice?.target || null}
        sourceProvider={transferChoice?.sourceProvider || "codex"}
        extend={transferChoice?.extend}
        onClose={() => {
          setTransferChoice(null);
          onModalOpenChange?.(false);
        }}
        onConfirm={async () => {
          if (!transferChoice) return;
          await post("/api/agents/account-transfer", {
            id: transferChoice.agentId,
            account_key: transferChoice.target.id,
            request_id: transferChoice.requestId,
          });
        }}
      />
      <Modal
        opened={!!disconnectChoice}
        zIndex={layerZIndex.modalAbovePopover}
        onClose={() => {
          if (!pending) setDisconnectChoice(null);
        }}
        title="Disconnect account"
        closeOnClickOutside={!pending}
        closeOnEscape={!pending}
        withCloseButton={!pending}
      >
        <h3 className="account-disconnect-identity">
          {disconnectChoice?.email || disconnectChoice?.label}
        </h3>
        <ul className="account-disconnect-effects">
          <li>New chats cannot use this account.</li>
          <li>Existing chats keep it. You can reconnect it later.</li>
          <li>
            Projects with this default need another account before new chats can
            start.
          </li>
        </ul>
        {disconnectsDefault && (
          <p className="account-replacement">
            {replacement
              ? `The application default changes to ${replacement.email || replacement.label}.`
              : "Connect another account before disconnecting the application default."}
          </p>
        )}
        <div className="account-confirm-actions">
          <Button
            variant="default"
            disabled={!!pending}
            onClick={() => setDisconnectChoice(null)}
          >
            Cancel
          </Button>
          <Button
            variant="filled"
            color="red"
            loading={pending === "disconnect"}
            disabled={!!pending || (disconnectsDefault && !replacement)}
            onClick={() => {
              const accountKey = disconnectChoice?.id;
              if (!accountKey) return;
              void action("disconnect", async () => {
                state.setData(
                  await post("/api/accounts/disconnect", {
                    account_key: accountKey,
                  }),
                );
                setDisconnectChoice(null);
              });
            }}
          >
            Disconnect account
          </Button>
        </div>
        {error && <p role="alert">{error}</p>}
      </Modal>
      <Modal
        opened={!!deleteChoice}
        zIndex={layerZIndex.modalAbovePopover}
        onClose={() => {
          if (!pending) {
            setDeleteChoice(null);
          }
        }}
        title="Delete account"
        closeOnClickOutside={!pending}
        closeOnEscape={!pending}
        withCloseButton={!pending}
      >
        <p>
          Delete <strong>{deleteChoice?.email || deleteChoice?.label}</strong>{" "}
          from this account list?
        </p>
        <p>
          New chats and account choices will no longer use it. Existing chats
          and active work keep their account identity. Native credentials and
          saved history are preserved.
        </p>
        {deletesDefault && (
          <p>
            {deleteReplacement
              ? `The application default changes to ${deleteReplacement.email || deleteReplacement.label}.`
              : "Add another connected account before deleting the application default."}
          </p>
        )}
        <Button
          variant="default"
          disabled={!!pending}
          onClick={() => {
            setDeleteChoice(null);
          }}
        >
          Cancel
        </Button>
        <Button
          color="red"
          loading={pending === "delete"}
          disabled={!!pending || (deletesDefault && !deleteReplacement)}
          onClick={() => {
            if (!deleteChoice || !deleteRequestId) return;
            void action("delete", async () => {
              state.setData(
                await confirmAccountDeleteRequest(
                  state.scope,
                  deleteChoice.id,
                  deleteRequestId,
                  (requestId) =>
                    post("/api/accounts/delete", {
                      account_key: deleteChoice.id,
                      request_id: requestId,
                    }),
                  storage,
                ),
              );
              setDeleteChoice(null);
              setDeleteRequestId("");
            });
          }}
        >
          Delete account
        </Button>
        {error && <p role="alert">{error}</p>}
      </Modal>
    </>
  );
}
