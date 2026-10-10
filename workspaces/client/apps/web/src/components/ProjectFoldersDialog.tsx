import { Tabs } from "@mantine/core";
import { useEffect, useState } from "react";
import { errorText, get } from "../api";
import {
  readProjectServers,
  type LocationProject,
  type ProjectServerChoice,
} from "../servers/projectLocations";
import ProjectLocationsSettings from "./ProjectLocationsSettings";

export default function ProjectFoldersDialog({
  project: initial,
  initialServer,
  saved,
}: {
  project: LocationProject;
  initialServer?: string;
  saved: () => Promise<void>;
}) {
  const [project, setProject] = useState(initial);
  const [servers, setServers] = useState<ProjectServerChoice[]>([]);
  const [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    void readProjectServers()
      .then((rows) => {
        if (active) setServers(rows);
      })
      .catch((failure) => {
        if (active) setError(errorText(failure));
      });
    return () => {
      active = false;
    };
  }, []);
  return (
    <Tabs defaultValue="folders">
      <Tabs.List>
        <Tabs.Tab value="folders">Folders</Tabs.Tab>
      </Tabs.List>
      <Tabs.Panel value="folders" pt="md">
        {error && <p role="alert">{error}</p>}
        <ProjectLocationsSettings
          key={project.locations?.map((row) => row.serverId).join(":")}
          project={project}
          servers={servers.length ? servers : undefined}
          initialServer={initialServer}
          saved={async () => {
            const result = await get("/api/projects");
            const next = result.items.find((row) => row.id === project.id);
            if (next) setProject(next);
            await saved();
          }}
        />
      </Tabs.Panel>
    </Tabs>
  );
}
