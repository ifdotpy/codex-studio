import {
  parseStudioPreferences,
  studioPreferencesStorageKey,
} from "../studioPreferences";
import { get, errorText } from "../api";
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
export function useServerFrame(
  data: Snapshot | null,
  opened: string | null,
  error: string,
  run: (command: ServerCommand) => void,
  unread: Set<string>,
  accounts: ServerAccount[],
) {
  const handler = useRef(run);
  handler.current = run;
  useEffect(() => {
    if (!serverViewId || window.parent === window) return;
    const receive = (event: MessageEvent) => {
      if (
        event.source !== window.parent ||
        event.origin !== serverParentOrigin ||
        event.data.serverId !== serverViewId
      )
        return;
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
    window.parent.postMessage(
      {
        kind: "studio-server-navigation",
        serverId: serverViewId,
        navigation: navigationSnapshot(
          data,
          opened,
          error,
          unread,
          data ? desktopAlerts(data) : [],
        ),
      },
      serverParentOrigin,
    );
  }, [data, opened, error, [...unread].join(":")]);
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
