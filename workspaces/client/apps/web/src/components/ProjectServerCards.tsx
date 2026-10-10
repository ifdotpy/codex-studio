import { Button, Text, SegmentedControl } from "@mantine/core";
import { useEffect, useState } from "react";
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
  const [system, setSystem] = useState(server?.system);
  const [mode, setMode] = useState<"layr" | "image" | "worktree">(
    defaultWorkspaceMode(server?.system),
  );
  const [models, setModels] = useState<Omit<
    NewChatSettings,
    "workspaceMode"
  > | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [platformError, setPlatformError] = useState("");
  useEffect(() => {
    const controller = new AbortController();
    setSystem(server?.system);
    setMode(defaultWorkspaceMode(server?.system));
    setPlatformError("");
    if (server && !server.system && !server.disabled) {
      void readNewChatResource(server.connection, "/api/ui-summary", {
        signal: controller.signal,
      })
        .then((value) => {
          if (controller.signal.aborted) return;
          setSystem(value.system);
          setMode(defaultWorkspaceMode(value.system));
        })
        .catch((error) => {
          if (!controller.signal.aborted) setPlatformError(String(error));
        });
    }
    return () => controller.abort();
  }, [selected, server?.system, server?.connection]);
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
              setMode(
                value === "layr"
                  ? "layr"
                  : value === "image"
                    ? "image"
                    : "worktree",
              )
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
