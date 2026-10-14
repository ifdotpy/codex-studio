import { Button, Text, SegmentedControl } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import {
  projectLocationOn,
  type LocationProject,
  type ProjectServerChoice,
  type ProjectLocation,
} from "../servers/projectLocations";
import "./project-locations.css";
import NewChatModelSettings from "./NewChatModelSettings";
import {
  defaultWorkspaceMode,
  readNewChatResource,
  type NewChatSettings,
} from "./newChatSettings";

export default function ProjectServerCards({
  project,
  servers,
  lastServer,
  onChoose,
  onAdd,
  onCancel,
}: {
  project: LocationProject;
  servers: ProjectServerChoice[];
  lastServer?: string;
  onChoose: (location: ProjectLocation, settings: NewChatSettings) => void;
  onAdd: (server: string) => void;
  onCancel: () => void;
}) {
  const [selected, setSelected] = useState(() => {
    const available = (id: string) => {
      const server = servers.find((row) => row.id === id);
      return (
        !!server && !server.disabled && !!projectLocationOn(project, server)
      );
    };
    const previous = servers.find(
      (row) => row.id === lastServer || row.serverId === lastServer,
    )?.id;
    return previous && available(previous)
      ? previous
      : servers.find((server) => available(server.id))?.id || "";
  });
  const server = servers.find((row) => row.id === selected);
  const location = server && projectLocationOn(project, server);
  // The parent rebuilds its server list often. Only another server or connection starts
  // a new read; the user's choices stay while the dialog is open.
  const connection = server?.connection;
  const connectionKey = JSON.stringify([
    selected,
    connection?.id,
    connection?.origin,
    connection?.credentialId,
  ]);
  const connectionRef = useRef(connection);
  connectionRef.current = connection;
  const [fetched, setFetched] = useState<{
    key: string;
    system?: string;
  } | null>(null);
  const system =
    server?.system ??
    (fetched?.key === connectionKey ? fetched.system : undefined);
  const fetchedKey = useRef("");
  fetchedKey.current = fetched?.key || "";
  const [modeChoice, setModeChoice] = useState<{
    server: string;
    mode: "layr" | "image" | "worktree";
  } | null>(null);
  const mode =
    modeChoice?.server === selected
      ? modeChoice.mode
      : defaultWorkspaceMode(system);
  const [models, setModels] = useState<Omit<
    NewChatSettings,
    "workspaceMode"
  > | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [platformError, setPlatformError] = useState("");
  const knownSystem = !!server?.system;
  const disabled = !!server?.disabled;
  const available = !!server;
  useEffect(() => {
    setPlatformError("");
    if (
      !available ||
      knownSystem ||
      disabled ||
      fetchedKey.current === connectionKey
    )
      return;
    const controller = new AbortController();
    void readNewChatResource(connectionRef.current, "/api/ui-summary", {
      signal: controller.signal,
    })
      .then((value) => {
        if (!controller.signal.aborted)
          setFetched({ key: connectionKey, system: value.system });
      })
      .catch((error) => {
        if (!controller.signal.aborted) setPlatformError(String(error));
      });
    return () => controller.abort();
  }, [connectionKey, available, knownSystem, disabled]);
  return (
    <div
      className="project-server-choice"
      onKeyDown={(event) => {
        if (event.key === "Escape" && !pickerOpen) {
          event.preventDefault();
          event.stopPropagation();
          onCancel();
        }
      }}
    >
      <div
        className="project-server-cards"
        role="group"
        aria-label="Chat server"
      >
        {servers.map((server) => {
          const folder = projectLocationOn(project, server);
          return (
            <div key={server.id} className="project-server-card">
              <button
                type="button"
                aria-pressed={selected === server.id}
                disabled={server.disabled || !folder}
                onClick={() => setSelected(server.id)}
              >
                <strong>{server.label}</strong>
                <Text component="span" size="xs" c="dimmed">
                  {server.disabled ? "Offline" : "Active"}
                </Text>
                {folder && <span>{folder.path}</span>}
              </button>
              {!folder && (
                <Button
                  variant="subtle"
                  size="xs"
                  disabled={server.disabled}
                  onClick={() => onAdd(server.id)}
                >
                  + Add folder on this server
                </Button>
              )}
            </div>
          );
        })}
      </div>
      {system === "Darwin" && (
        <div className="new-chat-run-mode">
          <Text component="label" id="new-chat-run-mode-label" size="sm">
            Workspace
          </Text>
          <SegmentedControl
            fullWidth
            aria-labelledby="new-chat-run-mode-label"
            value={mode}
            onChange={(value) =>
              setModeChoice({
                server: selected,
                mode:
                  value === "layr"
                    ? "layr"
                    : value === "image"
                      ? "image"
                      : "worktree",
              })
            }
            data={[
              { value: "layr", label: "layr" },
              { value: "image", label: "ASIF" },
              { value: "worktree", label: "worktree" },
            ]}
          />
        </div>
      )}
      {platformError && (
        <Text role="alert" c="red">
          {platformError}
        </Text>
      )}
      {server && location && (
        <NewChatModelSettings
          key={selected}
          server={server.connection}
          projectId={project.id}
          path={location.path}
          onOpenChange={setPickerOpen}
          onChange={setModels}
        />
      )}
      <div className="project-server-actions">
        <Button onClick={onCancel}>Cancel</Button>
        <Button
          variant="filled"
          color="indigo"
          disabled={
            !location ||
            !models ||
            !system ||
            !!platformError ||
            !!servers.find((row) => row.id === selected)?.disabled
          }
          onClick={() =>
            location &&
            models &&
            onChoose(
              { ...location, serverId: selected },
              {
                ...models,
                workspaceMode: system === "Darwin" ? mode : "worktree",
              },
            )
          }
        >
          Start chat
        </Button>
      </div>
    </div>
  );
}
