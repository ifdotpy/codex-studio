import { ActionIcon, Button, Menu } from "@mantine/core";
import { MoreHorizontal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { errorText, get } from "../api";
import {
  saveProjectLocation,
  ProjectReceiptRefused,
  type LocationProject,
  type ProjectServerChoice,
} from "../servers/projectLocations";
import ProjectDirectoryPicker from "./ProjectDirectoryPicker";
import {
  closeMenuFromTriggerOnEscape,
  restoreMenuTriggerFocusAfterClose,
  useMenuEscapeSuppression,
} from "./closeMenuFromTriggerOnEscape";
import "./project-locations.css";

export default function ProjectLocationsSettings({
  project,
  servers,
  initialServer,
  saved,
}: {
  project: LocationProject;
  servers?: ProjectServerChoice[];
  initialServer?: string;
  saved: () => Promise<void>;
}) {
  const [adding, setAdding] = useState(!!initialServer);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [openLocationMenu, setOpenLocationMenu] = useState<string | null>(null);
  const locationMenuTriggers = useRef(new Map<string, HTMLButtonElement>());
  const { suppressNextEscape, clearSuppression } = useMenuEscapeSuppression(
    openLocationMenu !== null,
  );
  const request = useRef<{
    id: string;
    server: string;
    path?: string;
    action: "add_location" | "remove_location";
  } | null>(null);
  const [heads, setHeads] = useState<Record<string, string>>({});
  useEffect(() => {
    for (const location of project.locations || []) {
      void get("/api/project-locations", {
        query: {
          project: project.id,
          server: location.serverId,
          action: "info",
        },
      })
        .then((row) =>
          setHeads((old) => ({
            ...old,
            [location.serverId]: row.gitHead || "",
          })),
        )
        .catch(() => {});
    }
  }, [project.id, project.locations]);
  const change = async (
    action: "add_location" | "remove_location",
    server: string,
    path?: string,
  ) => {
    const old = request.current;
    if (
      old &&
      (old.server !== server || old.path !== path || old.action !== action)
    )
      throw new Error(
        "Retry the pending location request before another change.",
      );
    request.current ||= { action, server, path, id: crypto.randomUUID() };
    await saveProjectLocation({
      action,
      server,
      path,
      project: project.id,
      request_id: request.current.id,
    }).catch((error: unknown) => {
      if (error instanceof ProjectReceiptRefused) request.current = null;
      throw error;
    });
    request.current = null;
    await saved();
  };
  return (
    <div className="project-locations" aria-label="Project folders">
      {(project.locations || []).map((location) => (
        <div className="project-location-row" key={location.serverId}>
          <div>
            <strong>
              {servers?.find(
                (row) => (row.serverId || row.id) === location.serverId,
              )?.label || location.serverId}
            </strong>
            <p>{location.path}</p>
            {location.requestedPath && (
              <small>Selected folder: {location.requestedPath}</small>
            )}
            {heads[location.serverId] && (
              <small>HEAD {heads[location.serverId]}</small>
            )}
          </div>
          <Menu
            opened={openLocationMenu === location.serverId}
            onChange={(nextOpened) => {
              setOpenLocationMenu((current) =>
                nextOpened
                  ? location.serverId
                  : current === location.serverId
                    ? null
                    : current,
              );
              if (!nextOpened)
                restoreMenuTriggerFocusAfterClose(
                  locationMenuTriggers.current.get(location.serverId) ?? null,
                );
            }}
            withinPortal={false}
            withInitialFocusPlaceholder={false}
          >
            <Menu.Target>
              <ActionIcon
                ref={(node) => {
                  if (node)
                    locationMenuTriggers.current.set(location.serverId, node);
                  else locationMenuTriggers.current.delete(location.serverId);
                }}
                className="project-location-actions"
                size={28}
                onClick={(event) =>
                  event.currentTarget.focus({ preventScroll: true })
                }
                data-mantine-stop-propagation={
                  openLocationMenu === location.serverId || suppressNextEscape
                    ? "true"
                    : undefined
                }
                onKeyDown={(event) =>
                  closeMenuFromTriggerOnEscape(
                    event,
                    openLocationMenu === location.serverId,
                    () =>
                      setOpenLocationMenu((current) =>
                        current === location.serverId ? null : current,
                      ),
                    suppressNextEscape,
                    clearSuppression,
                  )
                }
                onBlur={clearSuppression}
                aria-label={`Options for folder ${location.path}`}
                disabled={busy}
              >
                <MoreHorizontal size={16} />
              </ActionIcon>
            </Menu.Target>
            <Menu.Dropdown data-mantine-stop-propagation="true">
              <Menu.Item
                disabled={(project.locations?.length || 0) < 2}
                onClick={() => {
                  setBusy(true);
                  setError("");
                  void change("remove_location", location.serverId)
                    .catch((failure) => setError(errorText(failure)))
                    .finally(() => setBusy(false));
                }}
              >
                Remove
              </Menu.Item>
            </Menu.Dropdown>
          </Menu>
        </div>
      ))}
      {error && <p role="alert">{error}</p>}
      {adding ? (
        <ProjectDirectoryPicker
          serverChoices={servers}
          initialServer={initialServer}
          projectId={project.id}
          submitLabel="Add folder"
          onSelect={(path, server) => change("add_location", server, path)}
        />
      ) : (
        <Button variant="light" disabled={busy} onClick={() => setAdding(true)}>
          Add folder
        </Button>
      )}
    </div>
  );
}
