import { Button, Text } from "@mantine/core";
import { useState } from "react";
import {
  projectLocationOn,
  type LocationProject,
  type ProjectServerChoice,
  type ProjectLocation,
} from "../servers/projectLocations";
import "./project-locations.css";

export default function ProjectServerCards({
  project,
  servers,
  lastServer,
  onChoose,
  onAdd,
}: {
  project: LocationProject;
  servers: ProjectServerChoice[];
  lastServer?: string;
  onChoose: (location: ProjectLocation) => void;
  onAdd: (server: string) => void;
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
  return (
    <div className="project-server-choice">
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
      <Button
        disabled={
          !location || !!servers.find((row) => row.id === selected)?.disabled
        }
        onClick={() =>
          location && onChoose({ ...location, serverId: selected })
        }
      >
        Create chat
      </Button>
    </div>
  );
}
