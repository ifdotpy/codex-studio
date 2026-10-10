import {
  parseStudioPreferences,
  studioPreferencesStorageKey,
} from "../studioPreferences";
import { get, errorText, ApiError } from "../api";
import { executeSidebarRequest } from "./sidebarRpc";
import { desktopAlerts } from "../desktop/desktopAlerts";
import { useEffect, useRef } from "react";
import { serverViewId, serverParentOrigin } from "./environment";
import { publishServerAliases } from "./serverAliases";
import {
  isServerCommand,
  navigationSnapshot,
  type ServerAccount,
  type ServerCommand,
} from "./navigation";
import type { Snapshot } from "../types";
import { watchResourceConnection } from "../sync/resourceEvents";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import { serverStorageEventKey } from "./storage";
export function useServerFrame(
  data: Snapshot | null,
  opened: string | null,
  error: string,
  run: (command: ServerCommand) => void | Promise<void>,
  unread: Set<string>,
  accounts: ServerAccount[],
  creating = false,
) {
  const handler = useRef(run);
  handler.current = run;
  const latest = useRef({ data, opened, error, unread, creating });
  latest.current = { data, opened, error, unread, creating };
  useEffect(() => {
    if (!serverViewId || window.parent === window) return;
    const receive = (event: MessageEvent) => {
      if (
        !event.data ||
        typeof event.data !== "object" ||
        event.source !== window.parent ||
        event.origin !== serverParentOrigin ||
        event.data.serverId !== serverViewId
      )
        return;
      if (event.data.kind === "studio-sidebar-request") {
        if (typeof event.data.correlation !== "string") return;
        const reply = (result?: unknown, failure?: unknown) =>
          window.parent.postMessage(
            {
              kind: "studio-sidebar-result",
              serverId: serverViewId,
              correlation: event.data.correlation,
              result,
              ...(failure
                ? {
                    error: {
                      message: errorText(failure),
                      ...(failure instanceof ApiError
                        ? { status: failure.status, payload: failure.details }
                        : {}),
                    },
                  }
                : {}),
            },
            serverParentOrigin,
          );
        void executeSidebarRequest(
          event.data.value,
          async () => {
            await handler.current({ action: "refresh" });
            // Hidden frames suspend animation callbacks. Wait for React's next task.
            await new Promise<void>((resolve) => setTimeout(resolve, 0));
            const value = latest.current;
            return {
              ...navigationSnapshot(
                value.data,
                value.opened,
                value.error,
                value.unread,
                value.data ? desktopAlerts(value.data) : [],
              ),
              creating: value.creating,
            };
          },
          (command) => handler.current(command),
          latest.current.data?.token,
        ).then(
          (result) => reply(result),
          (failure) => reply(undefined, failure),
        );
        return;
      }
      if (event.data.kind === "studio-server-preferences") {
        const aliases = event.data.aliases;
        if (
          aliases &&
          typeof aliases === "object" &&
          Object.values(aliases).every(
            (value) => typeof value === "string" && /^[A-Z]{1,3}$/.test(value),
          )
        )
          publishServerAliases(aliases);
        try {
          const value = JSON.stringify(
            parseStudioPreferences(JSON.stringify(event.data.preferences)),
          );
          if (localStorage.getItem(studioPreferencesStorageKey) !== value) {
            localStorage.setItem(studioPreferencesStorageKey, value);
            window.dispatchEvent(
              new StorageEvent("storage", {
                key: studioPreferencesStorageKey,
                newValue: value,
              }),
            );
          }
        } catch {}
        return;
      }
      if (
        event.data.kind !== "studio-server-command" ||
        !isServerCommand(event.data.command)
      )
        return;
      const command = event.data.command as ServerCommand;
      if (command.action === "search") {
        void get("/api/search", { query: { q: command.query } })
          .then((value) => {
            window.parent.postMessage(
              {
                kind: "studio-server-search-results",
                serverId: serverViewId,
                requestId: command.requestId,
                results: value.results,
              },
              serverParentOrigin,
            );
          })
          .catch((failure: unknown) => {
            window.parent.postMessage(
              {
                kind: "studio-server-search-results",
                serverId: serverViewId,
                requestId: command.requestId,
                error: errorText(failure),
                results: [],
              },
              serverParentOrigin,
            );
          });
      } else handler.current(command);
    };
    window.addEventListener("message", receive);
    const stop = watchResourceConnection((status) => {
      window.parent.postMessage(
        { kind: "studio-server-status", serverId: serverViewId, status },
        serverParentOrigin,
      );
    });
    return () => {
      window.removeEventListener("message", receive);
      stop();
    };
  }, []);
  useEffect(() => {
    if (!serverViewId || window.parent === window) return;
    const publish = () =>
      window.parent.postMessage(
        {
          kind: "studio-server-navigation",
          serverId: serverViewId,
          navigation: {
            ...navigationSnapshot(
              data,
              opened,
              error,
              unread,
              data ? desktopAlerts(data) : [],
            ),
            creating,
          },
        },
        serverParentOrigin,
      );
    publish();
    const keys = new Set([
      `codex-project-compact:${data?.stateDir}`,
      `codex-project-tree:${data?.stateDir}`,
      `codex-sidebar-order:${data?.stateDir}`,
    ]);
    const preferencesChanged = (event: Event) => {
      if (!data) return;
      if (event instanceof CustomEvent && keys.has(event.detail)) publish();
      if (
        event instanceof StorageEvent &&
        (event.key === null ||
          keys.has(event.key) ||
          keys.has(serverStorageEventKey(event) || ""))
      )
        publish();
    };
    window.addEventListener(preferenceEvent, preferencesChanged);
    window.addEventListener("storage", preferencesChanged);
    return () => {
      window.removeEventListener(preferenceEvent, preferencesChanged);
      window.removeEventListener("storage", preferencesChanged);
    };
  }, [data, opened, error, [...unread].join(":"), creating]);
  useEffect(() => {
    if (!serverViewId || window.parent === window) return;
    const safeAccounts = accounts.map((account) => ({
      provider: account.provider,
      email: account.email,
      plan: account.plan,
      status: account.status,
      label: account.label,
      isDefault: account.isDefault,
    }));
    window.parent.postMessage(
      {
        kind: "studio-server-accounts",
        serverId: serverViewId,
        accounts: safeAccounts,
      },
      serverParentOrigin,
    );
  }, [accounts]);
}
